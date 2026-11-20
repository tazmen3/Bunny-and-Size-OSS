//! Opt-in native SMB qualification through the production scheduler and SQLite A.
//! Usage: smb_p1_probe SMB_URI [WAVES=2]. No connection or source write is made.
//! A wave admits at most eight 32-entry acquisition quanta (one for the first
//! root wave). Private databases live in an automatically removed tempdir;
//! stdout contains counts and states only, never personal names or paths.
#[cfg(target_os = "linux")]
mod native {
    use serde_json::json;
    use sortsize_core::{
        InventoryKind,
        acquisition::{Guard, Policy, SourceClass},
        analysis::SearchQuery,
        inventory_contract::*,
        volumes::smb,
    };
    use sortsize_store::{
        inventory::generation_a::{Pipeline, Repository},
        scheduler::{Session, State},
    };
    use std::{
        path::Path,
        thread,
        time::{Duration, Instant},
    };

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn context() -> AcquisitionContext {
        AcquisitionContext {
            inventory: InventoryId(1),
            source: SourceId(1),
            generation: GenerationId(1),
            scan: ScanId(1),
            policy: PolicyId(1),
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        }
    }

    fn guard(root: &Path) -> Result<Guard> {
        let guard = Guard::open(
            root,
            Policy {
                explicit_root: true,
                ..Default::default()
            },
        )?;
        if guard.evidence().0.class != SourceClass::Remote {
            return Err("explicitly selected SMB root must be classified Remote".into());
        }
        Ok(guard)
    }

    fn wave(session: &mut Session) -> Result<()> {
        session.control().pause(false);
        session.tick()?; // Admit once, then prohibit further admission while settling.
        session.control().pause(true);
        let started = Instant::now();
        loop {
            let status = session.tick()?;
            if status.in_flight == 0 {
                return Ok(());
            }
            if started.elapsed() > Duration::from_secs(5) {
                session.control().cancel();
                return Err("native operation did not settle within the lab deadline".into());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn seal(session: &mut Session) -> Result<ScopeCoverage> {
        session.control().cancel();
        let status = session.tick()?;
        if status.in_flight != 0 || status.state != State::Cancelled {
            return Err("qualification requires settled cancellation".into());
        }
        loop {
            if let Some(coverage) = session.finalize_step()? {
                return Ok(coverage);
            }
        }
    }

    fn counts(
        repo: &Repository,
        plan: &mut Pipeline,
        scratch: &Path,
    ) -> Result<(u64, u64, Option<u64>)> {
        std::fs::create_dir(scratch)?;
        let cap = repo
            .open_latest_with_plan(plan, &Default::default())?
            .ok_or("missing published generation")?;
        let entries = cap.as_store().entry_count(cap.generation())?;
        let logical = cap.as_store().totals(cap.generation())?.logical;
        let files = cap
            .query(
                &SearchQuery {
                    kind: Some(InventoryKind::File),
                    ..Default::default()
                },
                scratch,
                4 * 1024 * 1024,
                &Default::default(),
            )?
            .counts
            .matches;
        Ok((entries, files, logical))
    }

    pub fn run(stage: &mut &'static str) -> Result<()> {
        *stage = "input";
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.is_empty() || args.len() > 2 {
            return Err("SMB_URI [WAVES] required".into());
        }
        let logical = smb::parse_target(args[0].to_str().ok_or("URI required")?)?
            .share
            .ok_or("a share, not a server, is required")?;
        let waves: usize = args
            .get(1)
            .map(|s| s.to_str().ok_or("invalid waves"))
            .transpose()?
            .unwrap_or("2")
            .parse()?;
        if !(1..=3).contains(&waves) {
            return Err("waves must be between one and three".into());
        }
        *stage = "native-resolution";
        let root = smb::resolve_path(&logical)?;
        let reverse = smb::logical_path(&root).ok_or("missing reverse mount identity")?;
        if !smb::covers(&logical, &reverse) || !smb::covers(&reverse, &logical) {
            return Err("mount identity differs from the selected share".into());
        }
        if Guard::open(&root, Policy::default()).is_ok() {
            return Err("remote acquisition unexpectedly allowed without explicit action".into());
        }
        *stage = "explicit-guard";
        let opened = guard(&root)?;
        let temporary = tempfile::Builder::new()
            .prefix("bunny-smb-p1-private-")
            .tempdir()?;
        if !opened.store_outside(temporary.path())? {
            return Err("private SQLite state must remain outside the source".into());
        }
        *stage = "repository";
        let mut repo = Repository::open(
            &temporary.path().join("repository"),
            InventoryId(1),
            SourceId(1),
        )?;
        *stage = "g0-admission";
        let mut plan = Pipeline::plan()?;
        let c0 = repo.next_context(context())?;
        let store = repo.create_candidate_store(c0, &mut plan)?;
        let mut baseline = Session::new(store, c0, opened)?;
        if baseline.status().quanta != 0 || baseline.store().entry_count(c0.generation)? != 1 {
            return Err("session creation unexpectedly acquired entries".into());
        }
        *stage = "g0-acquisition";
        for _ in 0..waves {
            wave(&mut baseline)?;
        }
        *stage = "g0-seal";
        let coverage = seal(&mut baseline)?;
        let acquired = baseline
            .store()
            .entry_count(c0.generation)?
            .saturating_sub(1);
        let bound = 32 + (waves as u64 - 1) * 256;
        if acquired > bound {
            return Err("scheduler exceeded the qualification entry bound".into());
        }
        let errors = baseline.acquisition_error_count();
        let quanta = baseline.status().quanta;
        for _ in 0..3 {
            baseline.tick()?;
        }
        if baseline.status().quanta != quanta {
            return Err("sealed inventory restarted acquisition".into());
        }
        *stage = "g0-publication";
        let publication = repo.publish_with_plan(
            baseline.store(),
            c0.generation,
            &root,
            64 * 1024 * 1024,
            &Default::default(),
            &mut plan,
        )?;
        if !publication.committed() {
            return Err("SQLite A publication not committed".into());
        }
        *stage = "g0-query";
        let before = counts(&repo, &mut plan, &temporary.path().join("queries-0"))?;
        drop(baseline);
        drop(plan);

        // One further explicit candidate is interrupted after one bounded root
        // quantum. Its partial sealing must not replace the published G0.
        *stage = "g1-admission";
        let mut plan = Pipeline::plan()?;
        let c1 = repo.next_context(context())?;
        let store = repo.create_candidate_store(c1, &mut plan)?;
        let mut candidate = Session::new(store, c1, guard(&root)?)?;
        *stage = "g1-acquisition";
        wave(&mut candidate)?;
        *stage = "g1-seal";
        let candidate_coverage = seal(&mut candidate)?;
        let candidate_entries = candidate
            .store()
            .entry_count(c1.generation)?
            .saturating_sub(1);
        if candidate_entries > 32 {
            return Err("candidate exceeded one root quantum".into());
        }
        if repo
            .latest_context()?
            .is_none_or(|d| d.generation != c0.generation)
        {
            return Err("unpublished candidate changed the published generation".into());
        }
        *stage = "g0-preservation-query";
        let after = counts(&repo, &mut plan, &temporary.path().join("queries-1"))?;
        if before != after {
            return Err("published G0 changed during candidate G1".into());
        }
        drop(candidate);
        drop(plan);
        drop(repo);
        *stage = "private-cleanup";
        temporary.close()?; // Explicit cleanup before reporting success.
        println!(
            "{}",
            json!({
                "status":"Passed", "native_resolution":true, "reverse_share_identity":true,
                "implicit_remote_denied":true, "manual_acquisition":true,
                "waves":waves, "source_entry_bound":bound+32,
                "acquired_entries":acquired, "inventory_entries":before.0,
                "inventory_files":before.1, "known_logical_bytes":before.2,
                "coverage":format!("{coverage:?}"), "native_errors":errors,
                "sqlite_a_published":true, "g1_entries":candidate_entries,
                "g1_coverage":format!("{candidate_coverage:?}"), "g0_preserved":true,
                "no_automatic_restart":true, "private_database_removed":true,
                "contents_read":false
            })
        );
        Ok(())
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    {
        let mut stage = "startup";
        let Err(error) = native::run(&mut stage) else {
            return;
        };
        // Contract messages are authored static strings, never native paths.
        let contract = match error.downcast_ref::<sortsize_store::inventory::Error>() {
            Some(sortsize_store::inventory::Error::Contract(message)) => Some(*message),
            _ => None,
        };
        println!(
            "{}",
            serde_json::json!({"status":"Failed", "stage":stage,
            "contract":contract, "personal_paths_logged":false})
        );
    }
    // Never print native errors: they can contain personal source paths.
    #[cfg(not(target_os = "linux"))]
    println!("{{\"status\":\"Failed\",\"personal_paths_logged\":false}}");
    std::process::exit(1);
}
