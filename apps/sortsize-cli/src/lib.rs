use clap::{Parser, Subcommand, ValueEnum};
use sortsize_core::{
    CancellationToken, ConcurrencyOptions, Coverage, ScanOptions, ScanReport, TraversalLimits, scan,
};
use std::fs::OpenOptions;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

mod delimited;
mod entries_output;

#[derive(Parser)]
#[command(
    name = "bunny-and-size-cli",
    version,
    long_version = concat!("Bunny and Size ", env!("CARGO_PKG_VERSION")),
    about = "Bunny and Size — analyse du stockage en lecture seule"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Analyser un dossier local sans suivre les liens symboliques.
    Scan {
        path: PathBuf,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
        /// Résumé historique ou inventaire détaillé CSV/TSV.
        #[arg(long, value_enum, default_value = "summary")]
        scope: Scope,
        /// Budget des lignes de chaque tri externe (scope entries uniquement).
        #[arg(long)]
        sort_memory_bytes: Option<usize>,
        /// Parent du spool privé de tri externe (scope entries uniquement).
        #[arg(long)]
        temporary_directory: Option<PathBuf>,
        /// Écrire un nouveau fichier ; refuse d'écraser un fichier existant.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Glob relatif à la racine, répétable. Exemple : '**/.git/**'.
        #[arg(long)]
        exclude: Vec<String>,
        /// Profondeur maximale ; la racine est au niveau zéro.
        #[arg(long)]
        max_depth: Option<usize>,
        #[arg(long, default_value_t = 100)]
        max_errors: usize,
        /// Nombre maximal de dossiers en attente de parcours.
        #[arg(long, default_value_t = TraversalLimits::default().max_pending_directories)]
        max_pending_directories: usize,
        /// Budget des chemins natifs en attente, en octets (hors inventaire).
        #[arg(long, default_value_t = TraversalLimits::default().max_pending_path_bytes)]
        max_pending_path_bytes: usize,
        /// Nombre de dossiers analysés en parallèle (1 à 32, défaut prudent : 1).
        #[arg(long, default_value_t = 1)]
        workers: usize,
        /// Nombre maximal d'observations dans la file partagée du mode parallèle.
        #[arg(long, default_value_t = ConcurrencyOptions::default().max_buffered_observations)]
        max_buffered_observations: usize,
        /// Budget des chemins dans la file partagée du mode parallèle, en octets.
        #[arg(long, default_value_t = ConcurrencyOptions::default().max_buffered_path_bytes)]
        max_buffered_path_bytes: usize,
        /// Politique par défaut ; ce premier scanner ne suit jamais les liens.
        #[arg(long)]
        no_follow_symlinks: bool,
        /// Masquer les messages de progression sur stderr.
        #[arg(long)]
        quiet: bool,
        /// Progression sur stderr : auto (terminal), always ou never ; --quiet prime.
        #[arg(long, value_enum, default_value = "auto")]
        progress: ProgressMode,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
    Csv,
    Tsv,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Scope {
    Summary,
    Entries,
}

#[derive(Clone, Copy, ValueEnum)]
enum ProgressMode {
    Auto,
    Always,
    Never,
}

pub fn main_entry() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("Bunny and Size : {}", error.to_string().escape_debug());
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<u8, Box<dyn std::error::Error>> {
    let Command::Scan {
        path,
        format,
        scope,
        sort_memory_bytes,
        temporary_directory,
        output,
        exclude,
        max_depth,
        max_errors,
        max_pending_directories,
        max_pending_path_bytes,
        workers,
        max_buffered_observations,
        max_buffered_path_bytes,
        quiet,
        progress,
        ..
    } = cli.command;
    if scope == Scope::Entries && !matches!(format, Format::Csv | Format::Tsv) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--scope entries exige --format csv ou tsv",
        )
        .into());
    }
    if scope == Scope::Summary && (sort_memory_bytes.is_some() || temporary_directory.is_some()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Les options de tri externe exigent --scope entries",
        )
        .into());
    }
    let cancellation = CancellationToken::default();
    let signal_token = cancellation.clone();
    ctrlc::set_handler(move || signal_token.cancel())?;
    let options = ScanOptions {
        exclusions: exclude,
        max_depth,
        max_errors,
        traversal_limits: TraversalLimits {
            max_pending_directories,
            max_pending_path_bytes,
        },
        concurrency: ConcurrencyOptions {
            workers,
            max_buffered_observations,
            max_buffered_path_bytes,
        },
        ..Default::default()
    };
    let show_progress = !quiet
        && match progress {
            ProgressMode::Auto => io::stderr().is_terminal(),
            ProgressMode::Always => true,
            ProgressMode::Never => false,
        };
    if show_progress {
        eprintln!("Analyse démarrée ; Ctrl+C pour annuler.");
    }
    let mut last_progress = Instant::now();
    let on_progress = |progress: &sortsize_core::Progress| {
        if show_progress && last_progress.elapsed() >= Duration::from_millis(250) {
            eprintln!(
                "{} fichiers, {} dossiers, {} octets logiques, {} erreurs",
                progress.files, progress.directories, progress.logical_bytes, progress.errors_count
            );
            last_progress = Instant::now();
        }
    };
    if scope == Scope::Entries {
        let export = sortsize_core::entries::ExportOptions {
            sort_memory_bytes: sort_memory_bytes.unwrap_or(8 * 1024 * 1024),
            temporary_directory,
        };
        let separator = if matches!(format, Format::Csv) {
            b','
        } else {
            b'\t'
        };
        let report = if let Some(destination) = output {
            let mut stage = entries_output::StagedOutput::new(&destination)?;
            let report = {
                let mut writer = BufWriter::new(&mut stage);
                let report = sortsize_core::entries::export_entries(
                    &path,
                    &options,
                    &export,
                    &cancellation,
                    on_progress,
                    &mut writer,
                    separator,
                )?;
                writer.flush()?;
                report
            };
            stage.publish()?;
            report
        } else {
            let mut writer = BufWriter::new(io::stdout().lock());
            let report = sortsize_core::entries::export_entries(
                &path,
                &options,
                &export,
                &cancellation,
                on_progress,
                &mut writer,
                separator,
            )?;
            writer.flush()?;
            report
        };
        return Ok(coverage_code(report.coverage));
    }
    let mut report = scan(&path, &options, &cancellation, on_progress)?;
    delimited::canonicalize(&mut report);
    // Buffer only the bounded report, never a full file inventory.
    let mut data = Vec::new();
    write_report(&mut data, &report, format)?;
    if let Some(destination) = output {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(&data)?;
        writer.flush()?;
    } else {
        let mut stdout = io::stdout().lock();
        stdout.write_all(&data)?;
        stdout.flush()?;
    }
    Ok(coverage_code(report.coverage))
}

fn coverage_code(coverage: Coverage) -> u8 {
    match coverage {
        Coverage::Complete => 0,
        Coverage::Partial => 2,
        Coverage::Cancelled => 130,
    }
}

fn write_report(writer: &mut impl Write, report: &ScanReport, format: Format) -> io::Result<()> {
    match format {
        Format::Csv => delimited::write_report(writer, report, b','),
        Format::Tsv => delimited::write_report(writer, report, b'\t'),
        Format::Json => {
            serde_json::to_writer_pretty(&mut *writer, report).map_err(io::Error::other)?;
            writeln!(writer)
        }
        Format::Text => {
            writeln!(writer, "Source : {}", report.root.display.escape_debug())?;
            writeln!(writer, "État : {:?}", report.coverage)?;
            writeln!(writer, "Fichiers : {}", report.files)?;
            writeln!(writer, "Dossiers (racine incluse) : {}", report.directories)?;
            writeln!(
                writer,
                "Taille logique par chemin : {}{} octets",
                if report.coverage == Coverage::Complete {
                    ""
                } else {
                    "au moins "
                },
                report.logical_bytes
            )?;
            writeln!(writer, "Taille allouée / récupérable : non mesurée")?;
            writeln!(
                writer,
                "Liens symboliques ignorés : {}",
                report.symlinks_skipped
            )?;
            writeln!(writer, "Entrées exclues : {}", report.excluded_entries)?;
            writeln!(
                writer,
                "Dossiers limités en profondeur : {}",
                report.depth_limited_directories
            )?;
            writeln!(writer, "Durée : {} ms", report.duration_ms)?;
            writeln!(
                writer,
                "Dossiers non parcourus (budget de ressources) : {}",
                report.resource_limited_directories
            )?;
            writeln!(
                writer,
                "Parcours : au plus {} dossiers / {} octets de chemins en attente ; pics observés {} / {}",
                report.traversal_limits.max_pending_directories,
                report.traversal_limits.max_pending_path_bytes,
                report.traversal_stats.peak_pending_directories,
                report.traversal_stats.peak_pending_path_bytes
            )?;
            writeln!(
                writer,
                "Parallélisme : {} demandé(s), {} effectif(s) ; pic de listes ouvertes {}",
                report.concurrency.workers,
                report.traversal_stats.worker_threads,
                report.traversal_stats.peak_open_directories
            )?;
            writeln!(
                writer,
                "Erreurs : {} ({} détails omis)",
                report.errors_count, report.errors_omitted
            )?;
            for error in &report.errors {
                writeln!(
                    writer,
                    "  {} : {}",
                    error.path.display.escape_debug(),
                    error.message.escape_debug()
                )?;
            }
            Ok(())
        }
    }
}
