//! Read-only #195 protected-reference qualification of two explicitly prepared fixture trees.
//! Usage: protected_reference_lab ROOT_A ROOT_B NEW_STATE EXPECTED_FOLDER_GROUPS EXPECTED_REMOTE_SOURCES [unverified-roots]
//! Result expansion and pagination are checked against unchanged content-read counters.
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
    path::Path,
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
    let c = AcquisitionContext {
        inventory: InventoryId(id),
        source: SourceId(id),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let mut s = Session::new(
        Store::create(output, c.inventory, c.source, Durability::Retained)?,
        c,
        explicit(root)?,
    )?;
    let start = Instant::now();
    while s.tick()?.state != State::Idle {
        if start.elapsed() > Duration::from_secs(30) {
            s.control().cancel();
            return Err("small fixture acquisition exceeded 30 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    loop {
        if let Some(coverage) = s.finalize_step()? {
            if coverage != ScopeCoverage::Complete {
                return Err("fixture inventory incomplete".into());
            }
            break;
        }
    }
    Ok(s)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !(5..=6).contains(&args.len()) {
        return Err("ROOT_A ROOT_B NEW_STATE EXPECTED_FOLDER_GROUPS EXPECTED_REMOTE_SOURCES [unverified-roots]".into());
    }
    let a = Path::new(&args[0]);
    let b = Path::new(&args[1]);
    let output = Path::new(&args[2]);
    let expected_groups: u64 = args[3].to_str().ok_or("folder count")?.parse()?;
    let expected_remote: usize = args[4].to_str().ok_or("remote count")?.parse()?;
    let unverified = args.get(5).and_then(|a| a.to_str()) == Some("unverified-roots");
    if args.len() == 6 && !unverified {
        return Err("unknown optional flag".into());
    }
    local_state_location(output)?;
    let initial_guards = [explicit(a)?, explicit(b)?];
    let remote = initial_guards
        .iter()
        .filter(|g| g.evidence().0.class == SourceClass::Remote)
        .count();
    if remote != expected_remote {
        return Err(format!("native remote count {remote} != requested {expected_remote}").into());
    }
    std::fs::create_dir(output)?;
    let sa = acquire(a, &output.join("inventory-a"), 19501)?;
    let sb = acquire(b, &output.join("inventory-b"), 19502)?;
    let revisions = (
        sa.store().revision(GenerationId(1))?,
        sb.store().revision(GenerationId(1))?,
    );
    let quanta = (sa.status().quanta, sb.status().quanta);
    let cancel = CancellationToken::default();
    let mut qa = sa.store().start_query(
        GenerationId(1),
        a,
        &SearchQuery::default(),
        output,
        16 * 1024 * 1024,
    )?;
    let mut qb = sb.store().start_query(
        GenerationId(1),
        b,
        &SearchQuery::default(),
        output,
        16 * 1024 * 1024,
    )?;
    while !qa.advance(&cancel)? {}
    while !qb.advance(&cancel)? {}
    let scopes = || {
        vec![
            Scope {
                query: &qa,
                roots: vec![a.to_path_buf()],
                side: 0,
            },
            Scope {
                query: &qb,
                roots: vec![b.to_path_buf()],
                side: 1,
            },
        ]
    };
    let mut job = duplicates::prepare_scopes(
        scopes(),
        Mode::BetweenSources,
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
    job.verify_scoped(&guards, DuplicateOptions::default(), &cancel)?;
    let summary = serde_json::to_value(job.summary())?;
    if job.summary().folder_groups != expected_groups {
        return Err(format!("folder groups != {expected_groups}: {summary}").into());
    }
    if unverified && (job.summary().complete || job.summary().folder_groups != 0) {
        return Err("unverified root cannot have a complete folder badge".into());
    }
    let bytes_before = job.summary().bytes_read;
    let hashes_before = job.summary().files_hashed;
    if job.review_summary()?.files != 0 {
        return Err("new comparison must have no selected copy".into());
    }
    let mut review_results = Vec::new();
    for reference in [0_u8, 1] {
        job.set_protected_reference(Some(reference))?;
        let empty = job.review_summary()?;
        if empty.files != 0 || empty.logical_bytes != 0 {
            return Err("reference choice preselected copies".into());
        }
        let preview = job.review_preview()?;
        if job.review_summary()?.files != 0 {
            return Err("preview selected copies".into());
        }
        let groups = job.folder_groups(None)?;
        if preview.reason != duplicates::ReviewReason::Eligible {
            if preview.eligible_files != 0
                || preview.files != 0
                || job.select_all_review_copies().is_ok()
            {
                return Err("unverified comparison accepted review copies".into());
            }
            review_results.push(serde_json::json!({"reference":reference,"selected_files":0,"logical_bytes":0,"eligible_files":0,"initially_empty":true,"insufficient_evidence_excluded":true}));
            continue;
        }
        for group in groups {
            let target = duplicates::ReviewTarget::Folder(group.id);
            let protected = if reference == 0 { &group.a } else { &group.b };
            let copy = if reference == 0 { &group.b } else { &group.a };
            let protected_target = job
                .review_row_target(protected)?
                .ok_or("protected folder target")?;
            if job.review_state(protected_target)?.eligible {
                return Err("reference folder is eligible".into());
            }
            job.set_review_target(target, true)?;
            let selected = job.review_summary()?;
            if !job.review_state(target)?.selected
                || selected.files != group.files
                || selected.logical_bytes != group.logical_bytes
            {
                return Err("folder selection identity or logical count mismatch".into());
            }
            job.set_review_target(target, true)?;
            if job.review_summary()?.files != selected.files {
                return Err("folder selection double-counted parent".into());
            }
            let first = job.folder_page(group.id, None, false)?;
            let next = if first.more {
                Some(job.folder_page(group.id, first.last.as_ref(), false)?)
            } else {
                None
            };
            let page = next.as_ref().unwrap_or(&first);
            let child = page.rows.iter().find_map(|row| {
                let row = if reference == 0 { &row.b } else { &row.a };
                (row.kind == Kind::File).then_some(row)
            });
            if let Some(child) = child {
                if !child.path.starts_with(&copy.path) {
                    return Err("copy child on wrong side".into());
                }
                let child_target = job.review_row_target(child)?.ok_or("file target")?;
                if !job.review_state(child_target)?.selected {
                    return Err("offscreen child did not inherit selected folder".into());
                }
                job.set_review_target(child_target, false)?;
                let partial = job.review_summary()?;
                if !job.review_state(target)?.partial
                    || partial.files + 1 != selected.files
                    || partial.logical_bytes + child.logical.ok_or("file logical bytes")?
                        != selected.logical_bytes
                {
                    return Err("folder child exclusion is not partial or double-counts".into());
                }
                let _ = job.folder_page(group.id, None, false)?;
                let _ = job.file_page(None, false)?;
                if job.review_state(child_target)?.selected
                    || job.review_summary()?.files != partial.files
                {
                    return Err("presentation changed offscreen selection".into());
                }
                job.set_review_target(child_target, true)?;
                if !job.review_state(target)?.selected
                    || job.review_summary()?.logical_bytes != selected.logical_bytes
                {
                    return Err("child restore did not restore parent count".into());
                }
            }
            job.clear_review_selection()?;
        }
        let file_page = job.file_page(None, false)?;
        for (row, source) in file_page.rows.iter().zip(&file_page.sources) {
            let target = job
                .review_row_target(row)?
                .ok_or("standalone file target")?;
            let on_reference = *source
                == if reference == 0 {
                    SourceId(19501)
                } else {
                    SourceId(19502)
                };
            if on_reference {
                if job.review_state(target)?.eligible {
                    return Err("protected file is eligible".into());
                }
            } else if job.review_state(target)?.eligible {
                job.set_review_target(target, true)?;
                if !job.review_state(target)?.selected {
                    return Err("manual file selection failed".into());
                }
                job.set_review_target(target, false)?;
                if job.review_state(target)?.selected {
                    return Err("manual file deselection failed".into());
                }
                break;
            }
        }
        job.select_all_review_copies()?;
        let selected = job.review_summary()?;
        if selected.files != preview.eligible_files
            || selected.logical_bytes != preview.eligible_logical_bytes
        {
            return Err("assisted selection differs from global preview".into());
        }
        review_results.push(serde_json::json!({"reference":reference,"selected_files":selected.files,"logical_bytes":selected.logical_bytes,"eligible_files":preview.eligible_files,"initially_empty":true,"preview_did_not_select":true,"folder_child_checked":expected_groups>0,"all_pages_included":true}));
        job.set_protected_reference(Some(1 - reference))?;
        if job.review_summary()?.files != 0 {
            return Err("reference change did not reset selection".into());
        }
        job.clear_review_selection()?;
    }
    if job.summary().bytes_read != bytes_before || job.summary().files_hashed != hashes_before {
        return Err("selection/preview/paging read content".into());
    }
    let groups = job.folder_groups(None)?;
    if groups.len() as u64 != expected_groups {
        return Err("summary/group count mismatch".into());
    }
    let mut shown_files = 0;
    let mut folder_rows = Vec::new();
    let mut largest_page = 0;
    let mut pages = 0;
    for group in groups {
        let mut after = None;
        let mut entries = 0;
        loop {
            let page = job.folder_page(group.id, after.as_ref(), false)?;
            largest_page = largest_page.max(page.rows.len());
            if page.rows.len() > 100 {
                return Err("folder details exceeded 100-row bound".into());
            }
            entries += page.rows.len() as u64;
            pages += 1;
            if !page.more {
                break;
            }
            if page.last.is_none() {
                return Err("missing continuation cursor".into());
            }
            after = page.last;
        }
        shown_files += group.files;
        folder_rows.push(serde_json::json!({"id":group.id,"a":group.a.path,"b":group.b.path,"files":group.files,"logical_bytes":group.logical_bytes,"verified":group.verified,"detail_entries":entries}));
    }
    if shown_files != job.summary().folder_files {
        return Err("folder file count repeated descendants".into());
    }
    let mut cursor = None;
    let mut file_rows = 0;
    loop {
        let page = job.file_page(cursor.as_ref(), false)?;
        file_rows += page.rows.len();
        if !page.more {
            break;
        }
        if page.last.is_none() {
            return Err("missing file continuation cursor".into());
        }
        cursor = page.last;
    }
    if job.summary().bytes_read != bytes_before || job.summary().files_hashed != hashes_before {
        return Err("expansion/pagination read content".into());
    }
    drop(job);
    let mut limited = duplicates::prepare_scopes(
        scopes(),
        Mode::BetweenSources,
        output,
        32 * 1024 * 1024,
        &cancel,
    )?;
    limited.verify_scoped(
        &guards,
        DuplicateOptions {
            max_bytes_read: Some(1),
            ..Default::default()
        },
        &cancel,
    )?;
    if limited.summary().complete
        || !limited.summary().budget_limited
        || limited.summary().folder_groups != 0
    {
        return Err(format!(
            "limited run must not show folder badge: {:?}",
            limited.summary()
        )
        .into());
    }
    limited.set_protected_reference(Some(0))?;
    if limited.review_preview()?.eligible_files != 0 {
        return Err("insufficient evidence was eligible".into());
    }
    if limited.select_all_review_copies().is_ok() {
        return Err("incomplete comparison accepted assisted selection".into());
    }
    if limited.review_summary()?.files != 0 {
        return Err("incomplete comparison selected copies".into());
    }
    let limited_summary = serde_json::to_value(limited.summary())?;
    drop(limited);
    let cancelled = CancellationToken::default();
    cancelled.cancel();
    let mut stopped = duplicates::prepare_scopes(
        scopes(),
        Mode::BetweenSources,
        output,
        32 * 1024 * 1024,
        &cancel,
    )?;
    stopped.verify_scoped(&guards, Default::default(), &cancelled)?;
    if stopped.summary().complete
        || !stopped.summary().cancelled
        || stopped.summary().folder_groups != 0
    {
        return Err(format!(
            "cancelled run must not show folder badge: {:?}",
            stopped.summary()
        )
        .into());
    }
    stopped.set_protected_reference(Some(1))?;
    if stopped.review_preview()?.eligible_files != 0 {
        return Err("cancelled comparison eligible".into());
    }
    let stopped_summary = serde_json::to_value(stopped.summary())?;
    if revisions
        != (
            sa.store().revision(GenerationId(1))?,
            sb.store().revision(GenerationId(1))?,
        )
        || quanta != (sa.status().quanta, sb.status().quanta)
    {
        return Err("comparison changed inventory or restarted acquisition".into());
    }
    println!(
        "{}",
        serde_json::json!({"status":"PASS","root_a":a,"root_b":b,"native_remote_sources":remote,"comparison":summary,"folders":folder_rows,"file_detail_rows":file_rows,"folder_detail_pages":pages,"largest_folder_detail_page":largest_page,"presentation_content_reads":0,"presentation_hashes":0,"inventory_unchanged":true,"limited":limited_summary,"cancelled":stopped_summary,"three_sources_tested":false,"review":review_results,"incomplete_selection_excluded":true})
    );
    Ok(())
}
