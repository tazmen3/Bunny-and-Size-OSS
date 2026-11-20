//! Bounded read-only profile of certified Folder / Top100 preparation.
//! Opens an existing repository without acquisition or mutation of authority.
use sortsize_core::inventory_contract::*;
use sortsize_store::inventory::{
    folder_totals::FolderFold, generation_a::Repository, navigation::Mode,
};
use std::{path::PathBuf, time::Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if !(1..=2).contains(&args.len()) {
        return Err("REPOSITORY [MODE(auto|50000|250000|1000000|5000000|unlimited-count)]".into());
    }
    let governor = sortsize_core::resources::Governor::global();
    if let Some(key) = args.get(1) {
        let mode = MemoryMode::from_key(key.to_str().ok_or("ASCII mode required")?)
            .ok_or("invalid memory mode")?;
        governor.configure(mode, sortsize_core::resources::native_memory());
    }
    let start = Instant::now();
    let repo = Repository::open_readonly(&PathBuf::from(&args[0]))?;
    let cap = repo
        .open_latest(&Default::default())?
        .ok_or("empty repository")?;
    let certified = start.elapsed();
    let store = cap.as_store();
    let c = store.acquisition_context(cap.generation())?;
    let view = ViewContext {
        inventory: c.inventory,
        source: c.source,
        generation: c.generation,
        scan: c.scan,
        validation: c.validation,
        view: ViewId(1),
        epoch: ViewEpoch(1),
        query: None,
        presentation: PresentationEpoch(1),
    };
    let scratch = tempfile::tempdir()?;
    let mut fold = FolderFold::default();
    let t = Instant::now();
    let mut steps = 1;
    while !store.folder_fold_step_at(cap.generation(), &mut fold, scratch.path())? {
        steps += 1;
    }
    let fold_ms = t.elapsed().as_secs_f64() * 1000.;
    let t = Instant::now();
    let frame = store.navigation_frame(
        c,
        view,
        Mode::Folder {
            parent: EntryKey {
                inventory: c.inventory,
                generation: c.generation,
                entry: EntryId(0),
            },
            after: None,
        },
        cap.root(),
    )?;
    let folder_ms = t.elapsed().as_secs_f64() * 1000.;
    let t = Instant::now();
    let top = store.navigation_frame(c, view, Mode::Top100, cap.root())?;
    let top_ms = t.elapsed().as_secs_f64() * 1000.;
    let t = Instant::now();
    for row in &frame.rows {
        store.entry_properties(row.key)?;
        store.entry_modified(row.key)?;
    }
    let properties_ms = t.elapsed().as_secs_f64() * 1000.;
    let totals = store
        .folder_measure(c.generation, EntryId(0), &fold)?
        .ok_or("no totals")?;
    println!(
        "{}",
        serde_json::json!({"certification_ms":certified.as_secs_f64()*1000.,"fold_ms":fold_ms,"fold_steps":steps,"folder_ms":folder_ms,"top100_ms":top_ms,"properties_ms":properties_ms,"files":totals.files,"logical":totals.logical,"allocated":totals.allocated,"allocation_unknown":totals.allocation_unknown,"unknown":totals.unknown,"rows":frame.rows.len(),"top_rows":top.rows.len(),"scratch":store.folder_temporary_usage().map(|u|u.charged),"requested_mode":governor.usage().limits.requested.key(),"effective_limit_bytes":governor.usage().limits.bytes,"category_bytes":governor.usage().categories})
    );
    Ok(())
}
