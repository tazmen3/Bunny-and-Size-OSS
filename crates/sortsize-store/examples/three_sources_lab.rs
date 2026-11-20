//! Read-only comparison qualification for three explicitly prepared fixture roots.
//! Usage: three_sources_lab A B C NEW_STATE exact|ab|bc|unavailable REMOTE_COUNT
//! `unavailable` pauses after inventory and guard acquisition so the fixture
//! harness can temporarily disconnect C. The probe never modifies source data.
use sortsize_core::{
    CancellationToken,
    acquisition::{Guard, Policy, SourceClass, local_state_location},
    analysis::SearchQuery,
    duplicates::DuplicateOptions,
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::{
        Kind, Store,
        queries::duplicates::{self, Mode, Scope},
    },
    scheduler::{Session, State},
};
use std::{
    error::Error,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn explicit(root: &Path) -> Result<Guard> {
    Ok(Guard::open(
        root,
        Policy {
            explicit_root: true,
            ..Default::default()
        },
    )?)
}
fn acquire(root: &Path, output: &Path, id: u128) -> Result<Session> {
    let context = AcquisitionContext {
        inventory: InventoryId(id),
        source: SourceId(id),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut session = Session::new(
        Store::create(
            output,
            context.inventory,
            context.source,
            Durability::Retained,
        )?,
        context,
        explicit(root)?,
    )?;
    let start = Instant::now();
    while session.tick()?.state != State::Idle {
        if start.elapsed() > Duration::from_secs(60) {
            session.control().cancel();
            return Err("fixture inventory timeout".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    loop {
        if let Some(coverage) = session.finalize_step()? {
            if coverage != ScopeCoverage::Complete {
                return Err("fixture inventory incomplete".into());
            }
            break;
        }
    }
    Ok(session)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 6 {
        return Err("A B C NEW_STATE exact|ab|bc|unavailable REMOTE_COUNT".into());
    }
    let roots: Vec<PathBuf> = args[..3].iter().map(PathBuf::from).collect();
    let output = Path::new(&args[3]);
    let scenario = args[4].to_str().ok_or("scenario")?;
    if !["exact", "ab", "bc", "unavailable"].contains(&scenario) {
        return Err("scenario".into());
    }
    let expected_remote: usize = args[5].to_str().ok_or("remote count")?.parse()?;
    local_state_location(output)?;
    let initial_guards: Vec<_> = roots.iter().map(|p| explicit(p)).collect::<Result<_>>()?;
    let remote = initial_guards
        .iter()
        .filter(|g| g.evidence().0.class == SourceClass::Remote)
        .count();
    if remote != expected_remote {
        return Err(format!("native remote count {remote} != {expected_remote}").into());
    }
    let boundaries: Vec<_> = initial_guards
        .iter()
        .map(|g| {
            let (boundary, stamp) = g.evidence();
            serde_json::json!({"mount_path":boundary.path,"mount_root":boundary.root,
            "boundary_token":boundary.token,"class":format!("{:?}",boundary.class),
            "native_identity":format!("{:?}",stamp.identity)})
        })
        .collect();
    #[cfg(unix)]
    let native_devices: Vec<_> = {
        use std::os::unix::fs::MetadataExt;
        roots
            .iter()
            .map(|p| std::fs::metadata(p).map(|m| m.dev()))
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    #[cfg(not(unix))]
    let native_devices: Vec<u64> = Vec::new();
    std::fs::create_dir(output)?;
    let sessions: Vec<_> = roots
        .iter()
        .enumerate()
        .map(|(i, p)| {
            acquire(
                p,
                &output.join(format!("inventory-{i}")),
                193300 + i as u128,
            )
        })
        .collect::<Result<_>>()?;
    let revisions: Vec<_> = sessions
        .iter()
        .map(|s| s.store().revision(GenerationId(1)))
        .collect::<std::result::Result<_, _>>()?;
    let quanta: Vec<_> = sessions.iter().map(|s| s.status().quanta).collect();
    let cancel = CancellationToken::default();
    let filter = SearchQuery::default();
    let mut queries: Vec<_> = sessions
        .iter()
        .zip(&roots)
        .map(|(s, p)| {
            s.store()
                .start_query(GenerationId(1), p, &filter, output, 16 * 1024 * 1024)
        })
        .collect::<std::result::Result<_, _>>()?;
    for query in &mut queries {
        while !query.advance(&cancel)? {}
    }
    let scopes = |count: usize| {
        queries
            .iter()
            .zip(&roots)
            .enumerate()
            .take(count)
            .map(|(i, (query, root))| Scope {
                query,
                roots: vec![root.clone()],
                side: i as u8,
            })
            .collect()
    };
    let mut job = duplicates::prepare_scopes(
        scopes(3),
        Mode::BetweenThreeSources,
        output,
        32 * 1024 * 1024,
        &cancel,
    )?;
    if job.summary().bytes_read != 0 {
        return Err("preparation read content".into());
    }
    let guards: Vec<_> = job
        .guard_roots()
        .iter()
        .map(|p| explicit(p))
        .collect::<Result<_>>()?;
    if scenario == "unavailable" {
        println!("READY_TO_DISCONNECT_C");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if line.trim() != "CONTINUE" {
            return Err("disconnect harness acknowledgement missing".into());
        }
    }
    job.verify_scoped(&guards, DuplicateOptions::default(), &cancel)?;
    let summary = serde_json::to_value(job.summary())?;
    if scenario == "unavailable" {
        if job.summary().complete {
            return Err("unavailable C reported globally complete".into());
        }
    } else if !job.summary().complete || job.summary().folder_groups != 1 {
        return Err(format!("expected complete single folder group: {summary}").into());
    }
    let bytes = job.summary().bytes_read;
    let hashes = job.summary().files_hashed;
    let groups = job.folder_groups(None)?;
    let expected_locations = if scenario == "exact" { 3 } else { 2 };
    let mut folder_records = Vec::new();
    let mut detail_pages = 0;
    let mut largest_page = 0;
    for group in &groups {
        if scenario != "unavailable" && (2 + usize::from(group.c.is_some())) != expected_locations {
            return Err("wrong number of folder locations".into());
        }
        let mut after = None;
        loop {
            let page = job.folder_page(group.id, after.as_ref(), false)?;
            detail_pages += 1;
            largest_page = largest_page.max(page.rows.len());
            if page.rows.len() > 100 {
                return Err("detail page exceeded global 100-row bound".into());
            }
            if !page.more {
                break;
            }
            after = Some(page.last.ok_or("detail cursor missing")?);
        }
        folder_records.push(serde_json::json!({"id":group.id,"a":group.a.path,"a_side":group.a_side,
            "b":group.b.path,"b_side":group.b_side,"c":group.c.as_ref().map(|r|&r.path),
            "c_side":group.c_side,"files_per_tree":group.files,"logical_bytes_per_tree":group.logical_bytes}));
    }
    let mut review_records = Vec::new();
    for reference in 0..3_u8 {
        job.set_protected_reference(Some(reference))?;
        if job.review_summary()?.files != 0 {
            return Err("reference choice retained/preselected copies".into());
        }
        let preview = job.review_preview()?;
        let mut applied_files = 0;
        let mut applied_logical_bytes = 0;
        if job.review_summary()?.files != 0 {
            return Err("preview selected copies".into());
        }
        if scenario == "unavailable" {
            if preview.eligible_files != 0 || job.select_all_review_copies().is_ok() {
                return Err("incomplete comparison accepted assistance".into());
            }
        } else if (scenario == "bc" && reference == 0) || (scenario == "ab" && reference == 2) {
            if preview.eligible_files != 0 {
                return Err("absent reference offered copies".into());
            }
            for group in &groups {
                let state = job.review_state(duplicates::ReviewTarget::Folder(group.id))?;
                if state.eligible || format!("{:?}", state.reason) != "MissingReference" {
                    return Err(format!("missing reference reason: {state:?}").into());
                }
            }
        } else {
            let group = groups.first().ok_or("missing folder")?;
            let expected = group.files * (expected_locations - 1) as u64;
            if preview.eligible_files != expected {
                return Err(
                    format!("preview files {} != {expected}", preview.eligible_files).into(),
                );
            }
            job.select_all_review_copies()?;
            applied_files = job.review_summary()?.files;
            applied_logical_bytes = job.review_summary()?.logical_bytes;
            if job.review_summary()?.files != expected
                || job.review_summary()?.logical_bytes
                    != group.logical_bytes * (expected_locations - 1) as u64
            {
                return Err("selected copies counted incorrectly".into());
            }
            job.select_all_review_copies()?;
            if job.review_summary()?.files != expected {
                return Err("selection double counted".into());
            }
            for (side, row) in std::iter::once((group.a_side, &group.a))
                .chain(std::iter::once((group.b_side, &group.b)))
                .chain(group.c_side.zip(group.c.as_ref()))
            {
                let target = job.review_row_target(row)?.ok_or("folder target missing")?;
                let state = job.review_state(target)?;
                if side == reference && (state.eligible || state.selected) {
                    return Err("protected location selected".into());
                }
                if side != reference && !state.selected {
                    return Err("copy location omitted".into());
                }
            }
            let page = job.folder_page(group.id, None, false)?;
            let file = page.rows.iter().find_map(|detail| {
                std::iter::once((detail.a_side, &detail.a))
                    .chain(std::iter::once((detail.b_side, &detail.b)))
                    .chain(detail.c_side.zip(detail.c.as_ref()))
                    .find_map(|(side, row)| {
                        (side != reference && row.kind == Kind::File).then_some(row)
                    })
            });
            if let Some(file) = file {
                let target = job.review_row_target(file)?.ok_or("child target")?;
                job.set_review_target(target, false)?;
                if job.review_summary()?.files != expected - 1 {
                    return Err("partial child count mismatch".into());
                }
                let _ = job.folder_page(group.id, page.last.as_ref(), false)?;
                if job.review_summary()?.files != expected - 1 {
                    return Err("pagination lost partial selection".into());
                }
            }
        }
        let selected = job.review_summary()?;
        review_records.push(serde_json::json!({"reference":reference,"preview_reason":format!("{:?}",preview.reason),"eligible_files":preview.eligible_files,"applied_files_before_child_removal":applied_files,"applied_logical_bytes_before_child_removal":applied_logical_bytes,"selected_files_after_child_removal":selected.files,"selected_logical_bytes_after_child_removal":selected.logical_bytes}));
        job.clear_review_selection()?;
        if job.review_summary()?.files != 0 {
            return Err("clear retained copies".into());
        }
    }
    if job.summary().bytes_read != bytes || job.summary().files_hashed != hashes {
        return Err("presentation/review read source content".into());
    }
    drop(job);
    // A/B is a separate explicit mode and receives no C scope, guard or proof.
    let mut ab = duplicates::prepare_scopes(
        scopes(2),
        Mode::BetweenSources,
        output,
        32 * 1024 * 1024,
        &cancel,
    )?;
    if ab.guard_roots().len() != 2 || ab.guard_roots().iter().any(|p| p == &roots[2]) {
        return Err("C leaked into A/B guards".into());
    }
    let ab_guards: Vec<_> = ab
        .guard_roots()
        .iter()
        .map(|p| explicit(p))
        .collect::<Result<_>>()?;
    ab.verify_scoped(&ab_guards, Default::default(), &cancel)?;
    if !ab.summary().complete {
        return Err("return to A/B failed".into());
    }
    let ab_summary = serde_json::to_value(ab.summary())?;
    drop(ab);
    let resume_count = if scenario == "unavailable" { 2 } else { 3 };
    let resume_mode = if resume_count == 3 {
        Mode::BetweenThreeSources
    } else {
        Mode::BetweenSources
    };
    let resume_guards = if resume_count == 3 {
        &guards
    } else {
        &ab_guards
    };
    let stopped = CancellationToken::default();
    stopped.cancel();
    let mut cancelled = duplicates::prepare_scopes(
        scopes(resume_count),
        resume_mode,
        output,
        32 * 1024 * 1024,
        &cancel,
    )?;
    cancelled.verify_scoped(resume_guards, Default::default(), &stopped)?;
    if !cancelled.summary().cancelled || cancelled.summary().complete {
        return Err("cancellation did not reach terminal".into());
    }
    drop(cancelled);
    let mut resumed = duplicates::prepare_scopes(
        scopes(resume_count),
        resume_mode,
        output,
        32 * 1024 * 1024,
        &cancel,
    )?;
    resumed.verify_scoped(resume_guards, Default::default(), &cancel)?;
    if !resumed.summary().complete {
        return Err("resume after cancellation failed".into());
    }
    for (i, session) in sessions.iter().enumerate() {
        if session.store().revision(GenerationId(1))? != revisions[i]
            || session.status().quanta != quanta[i]
        {
            return Err("result consultations restarted acquisition".into());
        }
    }
    println!(
        "{}",
        serde_json::json!({"status":"PASS","scenario":scenario,"roots":roots,
        "native_devices":native_devices,"boundaries":boundaries,"native_remote_sources":remote,
        "comparison":summary,"folders":folder_records,"review":review_records,
        "detail_pages":detail_pages,"largest_detail_page":largest_page,"presentation_content_reads":0,
        "inventory_unchanged":true,"ab_excludes_c":true,"ab_comparison":ab_summary,
        "cancelled_then_resumed":true,"physical_windows_tested":false})
    );
    Ok(())
}
