//! Explicit, versioned cache/diff commands. Legacy CLI contracts are unchanged.
use crate::inventory_view::{View, is_repository, resolved_path};
use clap::{Args, Subcommand};
use sortsize_core::{
    CancellationToken, NativePath,
    acquisition::{Guard, Policy},
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::{
        Store, catalogue,
        differential::Diff,
        generation_a::{Pipeline, Repository},
        lifecycle,
    },
    scheduler::{Session, State},
};
use std::{
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Args)]
pub(crate) struct CacheArguments {
    #[command(subcommand)]
    operation: Operation,
}
#[derive(Subcommand)]
enum Operation {
    /// Catalogue local uniquement, sans accès aux racines enregistrées.
    List {
        #[arg(long)]
        directory: Option<PathBuf>,
    },
    /// Descripteur daté uniquement, sans réacquisition.
    Show {
        store: PathBuf,
        #[arg(long, default_value_t = 1)]
        generation: u64,
    },
    /// Copie cohérente opt-in dans un nouveau répertoire privé.
    Retain {
        store: PathBuf,
        destination: PathBuf,
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
    /// Nouvelle acquisition explicitement demandée et conservée localement.
    Measure {
        root: PathBuf,
        #[arg(long)]
        directory: Option<PathBuf>,
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
    /// Réénumération explicite de la candidate, aucun offset restauré.
    Resume {
        store: PathBuf,
        #[arg(long, default_value_t = 1)]
        generation: u64,
    },
    /// Nouvelle génération mesurée, ancienne référence conservée.
    Verify {
        store: PathBuf,
        #[arg(long, default_value_t = 1)]
        generation: u64,
    },
    /// Désactiver la conservation, purge locale après fermeture des lecteurs.
    Disable { store: PathBuf },
    /// Oubli du cache local uniquement. Aucune suppression de source.
    Forget {
        store: PathBuf,
        #[arg(long)]
        confirm: bool,
    },
    /// Maintenance locale des caches expirés, sans acquisition source.
    Prune {
        #[arg(long)]
        directory: Option<PathBuf>,
    },
    /// Remise explicite au consommateur historique simulé, copie durable possédée.
    Handoff {
        store: PathBuf,
        consumer: PathBuf,
        #[arg(long, default_value_t = 1)]
        generation: u64,
    },
}
#[derive(Args)]
pub(crate) struct DiffArguments {
    old: PathBuf,
    new: PathBuf,
    #[arg(long, default_value_t = 1)]
    old_generation: u64,
    #[arg(long, default_value_t = 1)]
    new_generation: u64,
    #[arg(long)]
    temporary_directory: PathBuf,
    /// Nouveau fichier JSONL, stdout par défaut ; aucun écrasement.
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long,default_value_t=1024*1024*1024_u64)]
    disk_bytes: u64,
}
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().min(i64::MAX as u64) as i64)
}
fn root(path: Option<PathBuf>) -> Result<PathBuf> {
    Ok(path.map_or_else(lifecycle::default_root, Ok)?)
}
fn descriptor(s: &catalogue::Saved) -> serde_json::Value {
    serde_json::json!({"cache_schema":1,"record":"measurement","inventory":format!("{:032x}",s.context.inventory.0),"source":format!("{:032x}",s.context.source.0),"generation":s.context.generation.0.to_string(),"store":NativePath::from(s.directory.as_path()),"root":NativePath::from(s.root.as_path()),"started_unix":s.started,"measured_unix":s.measured,"coverage":format!("{:?}",s.coverage),"logical_bytes":s.logical.map(|n|n.to_string()),"unknown":s.unknown.to_string(),"source_acquired":false})
}
fn print(value: &serde_json::Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}
fn acquire_into(
    mut session: Session,
    started: i64,
    mut publication: Option<(Repository, Pipeline)>,
) -> Result<u8> {
    let cancel = CancellationToken::default();
    let signal = cancel.clone();
    ctrlc::set_handler(move || signal.cancel())?;
    loop {
        if cancel.is_cancelled() {
            session.control().cancel();
            return Ok(130);
        }
        let status = session.tick()?;
        if status.state == State::StoreError {
            return Err("acquisition suspendue, données validées conservées".into());
        }
        if status.state == State::Idle
            && status.in_flight == 0
            && session.finalize_step()?.is_some()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let g = session.context().generation;
    let saved = session.store().saved(g)?;
    session
        .store_mut()
        .record_measurement(g, &saved.root, started, now())?;
    let mut value = descriptor(&session.store().saved(g)?);
    if let Some((repository, plan)) = publication.as_mut() {
        let growth = Repository::publication_budget(session.store())?;
        let result = match repository.publish_with_plan(
            session.store(),
            g,
            &saved.root,
            growth,
            &cancel,
            plan,
        ) {
            Ok(result) => result,
            Err(_) if cancel.is_cancelled() => {
                eprintln!(
                    "Publication interrompue ; artefacts locaux conservés, reprise explicite requise."
                );
                return Ok(130);
            }
            Err(error) => return Err(error.into()),
        };
        if !result.committed() {
            return Err(format!("génération publiée, sélection non confirmée ; reprise explicite requise : {result:?}").into());
        }
        value["store"] = serde_json::to_value(NativePath::from(repository.directory()))?;
    }
    value["source_acquired"] = true.into();
    drop(session);
    if let Some((repository, _)) = publication.as_mut() {
        repository.discard_candidate(g, &cancel)?;
    }
    print(&value)?;
    Ok(0)
}
/// Explicit cache operations may adopt a historical format. Consultation alone
/// never creates this companion and never scans the source.
fn adopt_legacy(
    path: &std::path::Path,
    selected: GenerationId,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let resolved = resolved_path(path, cancel)?;
    let legacy_path = if is_repository(&resolved) {
        let repository = Repository::open_readonly(&resolved)?;
        let pending = repository
            .directory()
            .join(format!(".pending-adopt-{:016x}", selected.0))
            .exists();
        if repository.published(selected)?.is_some()
            || repository.candidate_path(selected).exists() && !pending
        {
            return Ok(resolved);
        }
        match repository.legacy_origin()? {
            Some(original) => original,
            None => return Ok(resolved),
        }
    } else {
        path.to_owned()
    };
    // Reserve construction before opening the old RO connection, so a greedy
    // legacy cache cannot consume the publication worker's allowance.
    let mut plan = Pipeline::for_publication()?;
    let legacy = Store::certify_readonly(&legacy_path, cancel)?;
    let records = legacy.saved_records()?;
    let saved = legacy.saved(selected)?;
    let destination = Repository::companion_path(&legacy_path)?;
    let mut repository =
        Repository::open(&destination, saved.context.inventory, saved.context.source)?;
    repository.bind_legacy_origin(&legacy)?;
    repository.reserve_legacy_ids(&legacy)?;
    for saved in records.iter().filter(|record| record.measured.is_some()) {
        if repository.published(saved.context.generation)?.is_some() {
            continue;
        }
        drop(plan);
        repository.prepare_publication_retry(saved.context.generation, cancel)?;
        plan = Pipeline::for_publication()?;
        let result = repository.adopt_legacy_with_plan(
            &legacy,
            saved.context.generation,
            &saved.root,
            Repository::publication_budget(&legacy)?,
            cancel,
            &mut plan,
        )?;
        if !result.committed() {
            return Err(format!("adoption publiée sans sélection confirmée : {result:?}").into());
        }
        // Each publication consumes its cache slot and closes it before the
        // next snapshot, preserving the native governor for the old reader.
        plan = Pipeline::for_publication()?;
    }
    drop(plan);
    for saved in records.iter().filter(|record| record.measured.is_none()) {
        let pending = repository
            .directory()
            .join(format!(
                ".pending-adopt-{:016x}",
                saved.context.generation.0
            ))
            .exists();
        if repository.candidate_path(saved.context.generation).exists() && !pending {
            continue;
        }
        let mut plan = Pipeline::plan()?;
        let candidate = repository.adopt_candidate_cancellable(
            &legacy,
            saved.context.generation,
            &mut plan,
            cancel,
        )?;
        drop(candidate);
    }
    Ok(repository.directory().to_owned())
}
pub(crate) fn cache(a: CacheArguments) -> Result<u8> {
    match a.operation {
        Operation::List { directory } => {
            let c = catalogue::list(&root(directory)?, None)?;
            for s in c.rows {
                print(&descriptor(&s))?;
            }
            print(
                &serde_json::json!({"cache_schema":1,"record":"catalogue","unavailable":c.unavailable,"source_acquired":false}),
            )?;
        }
        Operation::Show { store, generation } => {
            let s = View::open(
                &store,
                GenerationId(generation),
                &CancellationToken::default(),
            )?;
            let mut value = descriptor(&s.store().saved(GenerationId(generation))?);
            value["store"] = serde_json::to_value(NativePath::from(store.as_path()))?;
            print(&value)?;
        }
        Operation::Retain {
            store,
            destination,
            days,
        } => {
            let cancel = CancellationToken::default();
            let resolved = if is_repository(&store) {
                store.clone()
            } else {
                let legacy = Store::certify_readonly(&store, &cancel)?;
                let first = legacy
                    .saved_records()?
                    .first()
                    .ok_or("aucune mesure enregistrée")?
                    .context
                    .generation;
                drop(legacy);
                adopt_legacy(&store, first, &cancel)?
            };
            let p = destination.parent().ok_or("destination sans parent")?;
            lifecycle::ensure_root(p)?;
            let repository = Repository::open_readonly(&resolved)?;
            let mut retained = repository.copy_to(&destination, &cancel)?;
            retained.set_retention(true, days)?;
            for saved in retained.descriptors()? {
                let view = retained.open_generation(saved.generation, &cancel)?;
                let mut value = descriptor(&view.as_store().saved(saved.generation)?);
                value["store"] = serde_json::to_value(NativePath::from(destination.as_path()))?;
                print(&value)?;
            }
        }
        Operation::Measure {
            root: source,
            directory,
            days,
        } => {
            let guard = Guard::open(
                &source,
                Policy {
                    explicit_root: true,
                    ..Default::default()
                },
            )?;
            let source = guard.root().to_owned();
            let directory = root(directory)?;
            if !guard.store_outside(&directory)? {
                return Err("cache must be outside acquired source".into());
            }
            lifecycle::ensure_root(&directory)?;
            lifecycle::account_root(&directory)?;
            let id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
                ^ (u128::from(std::process::id()) << 96);
            let c = AcquisitionContext {
                inventory: InventoryId(id),
                source: catalogue::source_hint(&source),
                generation: GenerationId(1),
                scan: ScanId(1),
                policy: PolicyId(1),
                mount: MountEpoch(1),
                run: RunEpoch(1),
                validation: ValidationEpoch(1),
            };
            let mut repository = Repository::open(
                &directory.join(format!("inv-{id:032x}")),
                c.inventory,
                c.source,
            )?;
            repository.set_retention(true, days)?;
            let mut plan = Pipeline::plan()?;
            let c = repository.next_context(c)?;
            let store = repository.create_candidate_store(c, &mut plan)?;
            let mut session = Session::new(store, c, guard)?;
            let started = now();
            session
                .store_mut()
                .record_acquisition(c, &source, started)?;
            session.store_mut().set_retention(true, days)?;
            return acquire_into(session, started, Some((repository, plan)));
        }
        Operation::Resume { store, generation } => {
            let cancel = CancellationToken::default();
            let store = adopt_legacy(&store, GenerationId(generation), &cancel)?;
            let (inventory, source) = Repository::inspect_identity(&store)?;
            let mut repository = Repository::open_for_recovery(&store, inventory, source)?;
            // Explicit recovery may reconcile a group renamed before a lost
            // pointer acknowledgement. No source access is needed for that.
            repository.prepare_publication_retry(GenerationId(generation), &cancel)?;
            if repository.published(GenerationId(generation))?.is_some() {
                let view = repository.open_generation(GenerationId(generation), &cancel)?;
                let mut value = descriptor(&view.as_store().saved(GenerationId(generation))?);
                value["store"] = serde_json::to_value(NativePath::from(store.as_path()))?;
                drop(view);
                repository.discard_candidate(GenerationId(generation), &cancel)?;
                print(&value)?;
                return Ok(0);
            }
            let mut plan = Pipeline::plan()?;
            let s = repository.open_candidate_cancellable(
                GenerationId(generation),
                &mut plan,
                &cancel,
            )?;
            let saved = s.saved(GenerationId(generation))?;
            if s.latest_sealed()? == Some(GenerationId(generation)) {
                // A crash after sealing does not require source acquisition.
                // Missing timestamps remain unknown instead of being refreshed.
                let result = repository.publish_with_plan(
                    &s,
                    GenerationId(generation),
                    &saved.root,
                    Repository::publication_budget(&s)?,
                    &cancel,
                    &mut plan,
                )?;
                if !result.committed() {
                    return Err(format!("publication locale non confirmée : {result:?}").into());
                }
                let mut value = descriptor(&saved);
                value["store"] = serde_json::to_value(NativePath::from(store.as_path()))?;
                drop(s);
                drop(plan);
                repository.discard_candidate(GenerationId(generation), &cancel)?;
                print(&value)?;
                return Ok(0);
            }
            let guard = Guard::open(
                &saved.root,
                Policy {
                    explicit_root: true,
                    ..Default::default()
                },
            )?;
            return acquire_into(
                Session::resume(s, GenerationId(generation), guard)?,
                saved.started,
                Some((repository, plan)),
            );
        }
        Operation::Verify { store, generation } => {
            let cancel = CancellationToken::default();
            let store = adopt_legacy(&store, GenerationId(generation), &cancel)?;
            if is_repository(&store) {
                let mut repository = Repository::open_existing(&store)?;
                // The selected reference may differ from CURRENT. Both remain
                // physically independent of the newly allocated candidate.
                let reference = repository.open_generation(GenerationId(generation), &cancel)?;
                let saved = reference.as_store().saved(GenerationId(generation))?;
                let guard = Guard::open(
                    &saved.root,
                    Policy {
                        explicit_root: true,
                        ..Default::default()
                    },
                )?;
                let c = repository.next_context(saved.context)?;
                let c = AcquisitionContext {
                    scan: ScanId(c.generation.0),
                    run: RunEpoch(c.generation.0),
                    mount: MountEpoch(c.generation.0),
                    validation: ValidationEpoch(c.generation.0),
                    ..c
                };
                let mut plan = Pipeline::plan()?;
                let candidate = repository.create_candidate_store(c, &mut plan)?;
                let mut session = Session::verify_candidate(
                    reference.as_store(),
                    saved.context.generation,
                    candidate,
                    c,
                    guard,
                )?;
                let started = now();
                session
                    .store_mut()
                    .record_acquisition(c, &saved.root, started)?;
                drop(reference);
                return acquire_into(session, started, Some((repository, plan)));
            }
            return Err("format de dépôt non reconnu après adoption".into());
        }
        Operation::Disable { store } => {
            let cancel = CancellationToken::default();
            let resolved = resolved_path(&store, &cancel)?;
            if is_repository(&resolved) {
                let mut repository = Repository::open_existing(&resolved)?;
                let days = repository.retention()?.days;
                repository.set_retention(false, days)?;
            } else {
                let mut s = Store::reopen(&store, Durability::Retained)?;
                let days = s.retention()?.days;
                s.set_retention(false, days)?;
            }
            print(
                &serde_json::json!({"cache_schema":1,"record":"retention_disabled","pending_local_purge":true,"source_acquired":false}),
            )?;
        }
        Operation::Forget { store, confirm } => {
            if !confirm {
                return Err("--confirm requis pour oublier ce cache local".into());
            }
            let cancel = CancellationToken::default();
            let resolved = resolved_path(&store, &cancel)?;
            if is_repository(&resolved) {
                Repository::open_existing(&resolved)?.forget_family(&cancel)?;
            } else {
                Store::reopen(&store, Durability::Retained)?.forget()?;
            }
            print(
                &serde_json::json!({"cache_schema":1,"record":"forgotten","source_acquired":false}),
            )?;
        }
        Operation::Prune { directory } => {
            let removed = catalogue::prune(&root(directory)?, now())?;
            print(
                &serde_json::json!({"cache_schema":1,"record":"maintenance","forgotten":removed,"source_acquired":false}),
            )?;
        }
        Operation::Handoff {
            store,
            consumer,
            generation,
        } => {
            let s = View::open(
                &store,
                GenerationId(generation),
                &CancellationToken::default(),
            )?;
            let lease = s.store().lease(GenerationId(generation))?;
            let receipt = catalogue::handoff(&lease, &consumer)?;
            print(
                &serde_json::json!({"cache_schema":1,"record":"history_receipt","generation":receipt.generation.0.to_string(),"replay":receipt.replay,"durably_owned":true}),
            )?;
        }
    }
    Ok(0)
}
pub(crate) fn diff(a: DiffArguments) -> Result<u8> {
    let cancel = CancellationToken::default();
    let signal = cancel.clone();
    ctrlc::set_handler(move || signal.cancel())?;
    let old = match View::open(&a.old, GenerationId(a.old_generation), &cancel) {
        Ok(view) => view,
        Err(_) if cancel.is_cancelled() => return Ok(130),
        Err(error) => return Err(error.into()),
    };
    let same = std::fs::canonicalize(&a.old)? == std::fs::canonicalize(&a.new)?;
    let new = if same && (a.old_generation == a.new_generation || matches!(&old, View::Legacy(_))) {
        None
    } else {
        Some(View::open(&a.new, GenerationId(a.new_generation), &cancel)?)
    };
    let mut d = Diff::start(
        old.store().lease(GenerationId(a.old_generation))?,
        new.as_ref()
            .unwrap_or(&old)
            .store()
            .lease(GenerationId(a.new_generation))?,
        &a.temporary_directory,
        a.disk_bytes,
    )?;
    while !d.advance(&cancel)? {}
    let mut out: Box<dyn Write> = if let Some(path) = a.output {
        Box::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?,
        )
    } else {
        Box::new(std::io::stdout().lock())
    };
    let (old, new) = d.measurements();
    serde_json::to_writer(
        &mut out,
        &serde_json::json!({"diff_schema":1,"record":"scope","old_generation":old.context.generation.0.to_string(),"new_generation":new.context.generation.0.to_string(),"old_measured_unix":old.measured,"new_measured_unix":new.measured,"source_acquired":false}),
    )?;
    writeln!(out)?;
    if d.summary().finished {
        let mut cursor = None;
        loop {
            let p = d.page(cursor.as_ref(), false)?;
            for e in &p.rows {
                serde_json::to_writer(
                    &mut out,
                    &serde_json::json!({"diff_schema":1,"record":"event","sequence":e.sequence.to_string(),"change":e.change,"old":e.old.as_deref().map(NativePath::from),"new":e.new.as_deref().map(NativePath::from)}),
                )?;
                writeln!(out)?;
            }
            if !p.more {
                break;
            }
            cursor = p.next;
        }
    }
    serde_json::to_writer(
        &mut out,
        &serde_json::json!({"diff_schema":1,"record":"summary","summary":d.summary()}),
    )?;
    writeln!(out)?;
    out.flush()?;
    Ok(if d.summary().cancelled { 130 } else { 0 })
}
