//! Opt-in V0.5 stored-query operations. The historical scanner stays independent.
use crate::inventory_view::View;
use clap::{Args, ValueEnum};
use sortsize_core::{CancellationToken, analysis::SearchQuery, inventory_contract::GenerationId};
use std::{io::Write, path::PathBuf};
#[derive(Clone, ValueEnum)]
enum Operation {
    Statistics,
    Export,
    /// Lecture de contenu explicitement demandée, résultats JSONL sur stdout.
    Duplicates,
}
#[derive(Args)]
pub(crate) struct Arguments {
    /// Magasin V0.5 déjà acquis. Seule l'opération duplicates lit du contenu source.
    store: PathBuf,
    /// Racine absolue ; duplicates y lit explicitement le contenu, les autres opérations restent locales.
    #[arg(long)]
    root: PathBuf,
    #[arg(long, default_value_t = 1)]
    generation: u64,
    #[arg(long, value_enum, default_value = "statistics")]
    operation: Operation,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    path_contains: Option<String>,
    #[arg(long)]
    extension: Option<String>,
    #[arg(long)]
    glob: Option<String>,
    #[arg(long)]
    min_bytes: Option<u64>,
    #[arg(long)]
    max_bytes: Option<u64>,
    #[arg(long)]
    case_sensitive: bool,
    /// Répertoire temporaire local pour la requête et les regroupements.
    #[arg(long)]
    temporary_directory: PathBuf,
    /// Nouveau fichier JSONL ; requis pour export, interdit pour statistics.
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long,default_value_t=2*1024*1024*1024_u64)]
    query_disk_bytes: u64,
    #[arg(long,default_value_t=512*1024*1024_u64)]
    groups_disk_bytes: u64,
    #[arg(long,default_value_t=2*1024*1024*1024_u64)]
    output_bytes: u64,
    /// Budget de lecture de contenu, uniquement pour duplicates.
    #[arg(long,default_value_t=1024*1024*1024_u64)]
    content_bytes: u64,
    #[arg(long,default_value_t=512*1024*1024_u64)]
    content_file_bytes: u64,
}
pub(crate) fn run(a: Arguments) -> Result<u8, Box<dyn std::error::Error>> {
    if !a.root.is_absolute() {
        return Err("--root doit être absolue".into());
    }
    if matches!(a.operation, Operation::Export) != a.output.is_some() {
        return Err("--output est requis uniquement pour --operation export".into());
    }
    let cancel = CancellationToken::default();
    let signal = cancel.clone();
    ctrlc::set_handler(move || signal.cancel())?;
    let store = match View::open(&a.store, GenerationId(a.generation), &cancel) {
        Ok(store) => store,
        Err(_) if cancel.is_cancelled() => return Ok(2),
        Err(error) => return Err(error.into()),
    };
    let search = SearchQuery {
        name: a.name,
        path: a.path_contains,
        extension: a.extension,
        glob: a.glob,
        min_bytes: a.min_bytes,
        max_bytes: a.max_bytes,
        case_sensitive: a.case_sensitive,
        ..Default::default()
    };
    let query = store.query(
        GenerationId(a.generation),
        &a.root,
        &search,
        &a.temporary_directory,
        a.query_disk_bytes,
        &cancel,
    )?;
    let complete = match a.operation {
        Operation::Statistics => {
            let stats = query.statistics(&a.temporary_directory, a.groups_disk_bytes, &cancel)?;
            let mut out = std::io::stdout().lock();
            serde_json::to_writer(&mut out, &stats.json())?;
            writeln!(out)?;
            out.flush()?;
            stats.complete
        }
        Operation::Export => {
            query
                .export_jsonl(a.output.as_deref().unwrap(), a.output_bytes, &cancel)?
                .complete
        }
        Operation::Duplicates => {
            use sortsize_core::{
                acquisition::{Guard, Policy},
                duplicates::DuplicateOptions,
            };
            if a.output_bytes < 4096 {
                return Err("--output-bytes doit réserver au moins 4096 octets".into());
            }
            let mut job =
                query.prepare_duplicates(&a.temporary_directory, a.groups_disk_bytes, &cancel)?;
            let guard = Guard::open(
                &a.root,
                Policy {
                    explicit_root: true,
                    ..Default::default()
                },
            )?;
            let mut out = std::io::stdout().lock();
            let mut written = 0u64;
            let mut line = |mut bytes: Vec<u8>| -> Result<bool, Box<dyn std::error::Error>> {
                bytes.push(b'\n');
                if written + bytes.len() as u64 > a.output_bytes - 2048 {
                    return Ok(false);
                }
                out.write_all(&bytes)?;
                written += bytes.len() as u64;
                Ok(true)
            };
            line(serde_json::to_vec(
                &serde_json::json!({"duplicates_schema":1,"record":"header","generation":a.generation.to_string(),"scope":"matched_regular_file_paths","root_supplied_by_caller":true,"query_complete":query.complete(),"content_requested":true}),
            )?)?;
            job.verify(
                &guard,
                DuplicateOptions {
                    max_bytes_read: Some(a.content_bytes),
                    max_file_bytes: Some(a.content_file_bytes),
                    ..Default::default()
                },
                &cancel,
            )?;
            let mut after = None;
            let mut transport_complete = true;
            'groups: loop {
                let groups = job.groups(after)?;
                if groups.is_empty() {
                    break;
                }
                for g in groups {
                    after = Some(g.id);
                    if cancel.is_cancelled()
                        || !line(serde_json::to_vec(
                            &serde_json::json!({"duplicates_schema":1,"record":"group","group":{"id":g.id.to_string(),"logical_bytes":g.logical_bytes.to_string(),"independent_members":g.independent_members.to_string(),"paths":g.paths.to_string()}}),
                        )?)?
                    {
                        transport_complete = false;
                        break 'groups;
                    }
                    let mut member_after = None;
                    loop {
                        let rows = job.members(g.id, member_after)?;
                        if rows.is_empty() {
                            break;
                        }
                        for row in rows {
                            member_after = Some(row.entry);
                            if cancel.is_cancelled() || !line(row.json_record(g.id)?)? {
                                transport_complete = false;
                                break 'groups;
                            }
                        }
                    }
                }
            }
            let complete = job.summary().complete && transport_complete && !cancel.is_cancelled();
            let trailer = serde_json::json!({"duplicates_schema":1,"record":"trailer","verification":job.summary(),"transport_complete":transport_complete,"cancelled":cancel.is_cancelled(),"complete":complete});
            serde_json::to_writer(&mut out, &trailer)?;
            writeln!(out)?;
            out.flush()?;
            complete
        }
    };
    Ok(if complete { 0 } else { 2 })
}
