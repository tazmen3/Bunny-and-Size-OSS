//! Presentation context, independent of scan generation and native data.
//! A scoped context lets isolated tests render English without process environment
//! changes. Worker messages retain raw values and render under the receiving UI's
//! context, so a delayed result cannot reinstate a former presentation generation.
use crate::locale::Language;
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    pub language: Language,
    pub generation: u64,
}
impl Default for Context {
    fn default() -> Self {
        Self {
            language: Language::French,
            generation: 0,
        }
    }
}
thread_local! {
    static CURRENT: Cell<Context> = Cell::new(Context::default());
}
impl Context {
    pub fn current() -> Self {
        CURRENT.with(Cell::get)
    }
    pub fn enter(self) -> Scope {
        Scope(
            CURRENT.with(|current| current.replace(self)),
            std::marker::PhantomData,
        )
    }
}
/// Thread-bound RAII restoration, including during unwinding and nested rendering.
pub struct Scope(Context, std::marker::PhantomData<std::rc::Rc<()>>);
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(self.0));
    }
}

pub(crate) fn text(source: &str) -> &str {
    text_in("rust", source)
}

pub(crate) fn text_in<'a>(context: &str, source: &'a str) -> &'a str {
    crate::catalog_generated::translation(Context::current().language.as_str(), context, source)
        .unwrap_or(source)
}

pub(crate) fn format(source: &str, arguments: &[String]) -> String {
    let template = text(source);
    assert_eq!(template.matches("{}").count(), arguments.len());
    let mut result =
        String::with_capacity(template.len() + arguments.iter().map(String::len).sum::<usize>());
    let mut pieces = template.split("{}");
    result.push_str(pieces.next().unwrap_or_default());
    for (argument, piece) in arguments.iter().zip(pieces) {
        // Never interpret an argument as a format string or translate native data.
        result.push_str(argument);
        result.push_str(piece);
    }
    result
}

pub(crate) const fn parameters(source: &str) -> usize {
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut count = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'{' && bytes[index + 1] == b'}' {
            count += 1;
        }
        index += 1;
    }
    count
}

macro_rules! tr {
    ($source:expr) => {
        crate::presentation::text($source)
    };
}
macro_rules! tr_in {
    ($context:literal, $source:literal) => {
        crate::presentation::text_in($context, $source)
    };
}
macro_rules! tr_format {
    ($source:literal $(, $argument:expr)* $(,)?) => {{
        const { assert!(crate::presentation::parameters($source) == (0usize $(+ { let _ = stringify!($argument); 1usize })*)); }
        crate::presentation::format($source, &[$(format!("{}", $argument)),*])
    }};
}

macro_rules! tr_count {
    ($singular:literal, $plural:literal, $count:expr) => {
        crate::presentation::count($singular, $plural, $count)
    };
}
pub(crate) fn count(singular: &str, plural: &str, count: u64) -> String {
    let source = if count == 1 { singular } else { plural };
    let template = crate::catalog_generated::plural(
        Context::current().language.as_str(),
        "rust",
        singular,
        count,
    )
    .unwrap_or(source);
    template.replace("{n}", &crate::number_format::integer(count.into()))
}
pub(crate) fn files(count: u64) -> String {
    tr_count!("{n} file", "{n} files", count)
}
pub(crate) fn folders(count: u64) -> String {
    tr_count!("{n} folder", "{n} folders", count)
}
pub(crate) fn errors(count: u64) -> String {
    tr_count!("{n} error", "{n} errors", count)
}

#[derive(Default)]
pub(crate) enum Status {
    #[default]
    Idle,
    EnterPath,
    Scanning,
    Listing,
    Listed(sortsize_core::Coverage),
    ListingFailed,
    Cancelling,
    StartFailed,
    Failed,
    Completed(sortsize_core::Coverage),
}
impl Status {
    pub(crate) fn render(&self) -> &'static str {
        match self {
            Self::Idle => tr!("Choose a folder to scan."),
            Self::EnterPath => tr!("Enter the path of a folder to scan."),
            Self::Scanning => tr!("Scan in progress…"),
            Self::Listing => tr!("Additional analysis in progress…"),
            Self::Listed(sortsize_core::Coverage::Complete) => tr!("Additional analysis complete"),
            Self::Listed(sortsize_core::Coverage::Partial) => {
                tr!("Additional analysis — partial results")
            }
            Self::Listed(sortsize_core::Coverage::Cancelled) => {
                tr!("Additional analysis cancelled — partial results retained")
            }
            Self::ListingFailed => tr!("Additional analysis failed."),
            Self::Cancelling => tr!("Cancellation requested…"),
            Self::StartFailed => tr!("Could not start the scan."),
            Self::Failed => tr!("Scan failed."),
            Self::Completed(sortsize_core::Coverage::Complete) => tr!("Scan complete"),
            Self::Completed(sortsize_core::Coverage::Partial) => {
                tr!("Partial scan — some entries were not traversed")
            }
            Self::Completed(sortsize_core::Coverage::Cancelled) => {
                tr!("Scan cancelled — partial results retained")
            }
        }
    }
}
#[derive(Default)]
pub(crate) enum Diagnostic {
    #[default]
    None,
    Native(String),
    PickerStart(String),
    PickerStopped,
    VolumeBlocked,
    Failure(crate::worker::Failure),
    Report(std::sync::Arc<crate::worker::ScanPresentation>),
}
impl Diagnostic {
    pub(crate) fn render(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::VolumeBlocked => tr!("Automatic scan unavailable: select an accessible local source. Other paths require an explicit scan.").into(),
            Self::Native(value) => value.clone(),
            Self::PickerStart(error) => tr_format!("Could not open the picker: {}", error),
            Self::PickerStopped => {
                tr!("The picker stopped without a result. Try again or enter the path manually.")
                    .into()
            }
            Self::Failure(failure) => failure.render(),
            Self::Report(report) => report.errors(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_contexts_restore_on_unwind_and_are_thread_isolated() {
        let before = Context::current();
        let english = Context {
            language: Language::English,
            generation: 20,
        };
        let _scope = english.enter();
        assert_eq!(Context::current(), english);
        let _ = std::panic::catch_unwind(|| {
            let _nested = Context {
                language: Language::French,
                generation: 21,
            }
            .enter();
            panic!("test scoped restoration");
        });
        assert_eq!(Context::current(), english);
        assert_eq!(
            std::thread::spawn(Context::current).join().unwrap(),
            Context::default()
        );
        drop(_scope);
        assert_eq!(Context::current(), before);
    }
}

#[derive(Default)]
pub(crate) enum OwnerStatus {
    #[default]
    Idle,
    Unavailable,
    Resolving,
    Page {
        pending: usize,
        waiting: bool,
    },
}
impl OwnerStatus {
    pub(crate) fn render(&self) -> String {
        match self {
            Self::Idle => String::new(),
            Self::Unavailable => tr!("Resolution unavailable; native identifiers retained.").into(),
            Self::Resolving => tr!("Resolving page names…").into(),
            Self::Page { waiting: true, .. } => {
                tr!("Resolution suspended: waiting for previous provider or service unavailable.")
                    .into()
            }
            Self::Page { pending, .. } if *pending > 0 => tr_count!(
                "Name resolution: {n} identity remaining on this page.",
                "Name resolution: {n} identities remaining on this page.",
                *pending as u64
            ),
            Self::Page { .. } => {
                tr!("Page names updated; native identifiers retained as fallback.").into()
            }
        }
    }
}

/// Human page range; never used as an index or an export field.
pub(crate) fn page(first: usize, last: usize) -> String {
    format!(
        "{}–{}",
        crate::number_format::integer(first as u128),
        crate::number_format::integer(last as u128)
    )
}

#[cfg(test)]
mod multilingual_tests {
    use super::*;
    #[test]
    fn all_catalogs_plural_boundaries_context_and_native_parameters() {
        for (language, expected, singular, plural) in [
            (Language::English, "1 file", "1 file", "0 files"),
            (Language::French, "1 fichier", "1 fichier", "0 fichier"),
            (Language::German, "1 Datei", "1 Datei", "0 Dateien"),
            (Language::Spanish, "1 archivo", "1 archivo", "0 archivos"),
            (
                Language::Portuguese,
                "1 ficheiro",
                "1 ficheiro",
                "0 ficheiros",
            ),
        ] {
            let _scope = Context {
                language,
                generation: 7,
            }
            .enter();
            assert_eq!(files(1), expected);
            assert_eq!(files(1), singular);
            assert_eq!(files(0), plural);
            for n in [0, 1, 2, 7, u64::MAX] {
                let label = files(n);
                assert!(label.starts_with(&crate::number_format::integer(n.into())));
                assert!(!label.contains("{n}"));
                let errors = errors(n);
                assert!(!errors.contains("{n}"));
            }
            let native = "Éva-日本語-{n}-{}-UID-SID-C:\\opaque";
            assert!(
                format("Could not access {}: {}", &[native.into(), native.into()]).contains(native)
            );
            assert_eq!(text_in("missing", "Unknown"), "Unknown");
        }
        let _scope = Context {
            language: Language::Portuguese,
            generation: 8,
        }
        .enter();
        assert_eq!(text("Unknown"), "Desconhecido");
        assert_eq!(text_in("capacity", "Unknown"), "Desconhecida");
        assert_eq!(files(2), "2 ficheiros");
        assert_eq!(files(7), "7 ficheiros");
    }
}
