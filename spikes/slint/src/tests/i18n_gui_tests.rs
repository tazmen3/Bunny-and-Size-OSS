use super::*;
use crate::{locale::Language, presentation::Context};

#[test]
fn english_and_french_raw_worker_late_results_pages_owner_volumes_and_recovery() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let fixture = TempDir::new().unwrap();
    for i in 0..2_007 {
        fs::write(fixture.path().join(format!("été-{i:04}")), b"x").unwrap();
    }
    for (language, complete, file, absent) in [
        (Language::English, "Scan complete", "File", "Not collected"),
        (
            Language::French,
            "Analyse terminée",
            "Fichier",
            "Non collectée",
        ),
    ] {
        let context = Context {
            language,
            generation: 7,
        };
        let _scope = context.enter();
        let window = crate::new_window().unwrap();
        let controller = connect(&window);
        window.set_scan_path(fixture.path().to_str().unwrap().into());
        window.invoke_start_scan();
        // Worker is always started independently of the harness's UI context.
        // Delay receipt until another presentation generation is current.
        controller.borrow_mut().presentation.generation = 8;
        wait(&window, &controller);
        assert_eq!(window.get_status(), complete);
        assert!(
            window
                .get_summary()
                .contains(if language == Language::English {
                    "2,007 files"
                } else {
                    "2\u{202f}007 fichiers"
                })
        );
        assert_eq!(window.get_details().row_count(), 2_000);
        assert_eq!(window.get_details().row_data(0).unwrap().note, file);
        assert_eq!(window.get_details().row_data(0).unwrap().allocated, absent);
        assert_eq!(window.get_top_rows().row_count(), 100);
        let ids = window
            .get_details()
            .iter()
            .map(|r| r.node_id)
            .collect::<Vec<_>>();
        window.invoke_change_page(true, true);
        assert_eq!(window.get_details().row_count(), 7);
        window.invoke_change_page(true, false);
        assert_eq!(
            ids,
            window
                .get_details()
                .iter()
                .map(|r| r.node_id)
                .collect::<Vec<_>>()
        );
        // Re-render from retained raw state. This is a
        // harness projection probe, not the B2 global locale transaction.
        let new_context = Context {
            language: if language == Language::English {
                Language::French
            } else {
                Language::English
            },
            generation: 9,
        };
        let _projection = new_context.enter();
        let mut state = controller.borrow_mut();
        state.presentation = new_context;
        let generation = state.generation;
        let owner_page = state.owner_page;
        state
            .tree
            .as_mut()
            .unwrap()
            .prepare_presentation(new_context);
        state.show_rows(&window);
        assert_eq!(state.generation, generation);
        assert_eq!(state.owner_page, owner_page);
        assert!(state.task.is_none());
        assert_eq!(
            ids,
            window
                .get_details()
                .iter()
                .map(|r| r.node_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            window.get_details().row_data(0).unwrap().note,
            if language == Language::English {
                "Fichier"
            } else {
                "File"
            }
        );
        state.presentation = context;
        drop(state);
        drop(_projection);
        window.set_scan_path(fixture.path().join("missing").to_str().unwrap().into());
        window.invoke_start_scan();
        wait(&window, &controller);
        assert_eq!(
            window.get_status(),
            if language == Language::English {
                "Scan failed."
            } else {
                "Analyse impossible."
            }
        );
        assert!(
            window
                .get_errors()
                .starts_with(if language == Language::English {
                    "Could not access "
                } else {
                    "Impossible d'accéder à "
                })
        );
        window.set_scan_path(fixture.path().to_str().unwrap().into());
        window.invoke_start_scan();
        wait(&window, &controller);
        assert_eq!(window.get_status(), complete);
        assert_eq!(window.get_details().row_data(0).unwrap().note, file);
        window.invoke_start_scan();
        window.invoke_cancel_scan();
        wait(&window, &controller);
        assert!(!window.get_busy());
        assert!(
            window
                .get_status()
                .starts_with(if language == Language::English {
                    "Scan cancelled"
                } else {
                    "Analyse annulée"
                })
        );
    }
}

#[test]
fn all_metadata_absences_partial_cancelled_counts_parameters_and_fallbacks() {
    use sortsize_core::{Coverage, MetadataValue};
    for language in [Language::English, Language::French] {
        let _scope = Context {
            language,
            generation: 1,
        }
        .enter();
        for n in [0, 1, 2, 7] {
            let expected = if language == Language::French {
                if n <= 1 { "fichier" } else { "fichiers" }
            } else if n == 1 {
                "file"
            } else {
                "files"
            };
            assert_eq!(presentation::files(n), format!("{n} {expected}"));
        }
        for (value, en, fr) in [
            (MetadataValue::Unknown, "Unknown", "Inconnue"),
            (
                MetadataValue::Unsupported,
                "Unsupported",
                "Non prise en charge",
            ),
            (MetadataValue::AccessDenied, "Access denied", "Accès refusé"),
            (
                MetadataValue::NotCollected,
                "Not collected",
                "Non collectée",
            ),
        ] {
            assert_eq!(
                crate::allocation_label::format_value(value, units::Units::Binary),
                if language == Language::English {
                    en
                } else {
                    fr
                }
            );
        }
        assert_eq!(
            presentation::text_in("capacity", "Unknown"),
            if language == Language::English {
                "Unknown"
            } else {
                "Inconnu"
            }
        );
        assert_eq!(
            presentation::text_in("capacity", "Unsupported"),
            if language == Language::English {
                "Unsupported"
            } else {
                "Non pris en charge"
            }
        );
        for coverage in [Coverage::Complete, Coverage::Partial, Coverage::Cancelled] {
            assert!(
                !presentation::Status::Completed(coverage)
                    .render()
                    .is_empty()
            );
        }
        assert_eq!(
            presentation::OwnerStatus::Resolving.render(),
            if language == Language::English {
                "Resolving page names…"
            } else {
                "Résolution des noms de la page…"
            }
        );
        assert_eq!(
            presentation::OwnerStatus::Page {
                pending: 0,
                waiting: false
            }
            .render(),
            if language == Language::English {
                "Page names updated; native identifiers retained as fallback."
            } else {
                "Noms de la page actualisés ; identifiants natifs conservés en secours."
            }
        );
        for count in [0, 1, 2, 7] {
            let status = presentation::OwnerStatus::Page {
                pending: count,
                waiting: false,
            }
            .render();
            if count > 0 {
                assert!(status.contains(if language == Language::English {
                    "identit"
                } else {
                    "identité"
                }));
            }
        }
        let opaque = "été-{n}-{}-SID-S-1-5-18-UID-1000";
        assert!(
            presentation::format("Could not open the picker: {}", &[opaque.into()])
                .ends_with(opaque)
        );
        assert_eq!(
            presentation::text("Deliberately missing source message"),
            "Deliberately missing source message"
        );
    }
    let _scope = Context {
        language: Language::German,
        generation: 2,
    }
    .enter();
    assert_eq!(presentation::files(0), "0 Dateien");
}

#[test]
fn fifty_thousand_raw_nodes_reprojection_keeps_identity_and_measures_bounded_work() {
    let mut tree = owner_gui_tests::synthetic_tree(50_000);
    let key = tree.owner_page_key();
    let mut samples = Vec::new();
    for generation in 0..40 {
        let context = Context {
            language: if generation % 2 == 0 {
                Language::English
            } else {
                Language::French
            },
            generation,
        };
        let start = Instant::now();
        tree.prepare_presentation(context);
        let rows = tree.details().0;
        assert_eq!(rows.len(), 2_000);
        assert_eq!(
            rows[0].note,
            if context.language == Language::English {
                "File"
            } else {
                "Fichier"
            }
        );
        assert!(tree.top_rows.len() <= 100);
        assert_eq!(tree.owner_page_key(), key);
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    eprintln!(
        "B1_PROJECTION nodes=50000 page=2000 top_limit=100 iterations=40 p95_ms={:.3} max_ms={:.3}",
        samples[37], samples[39]
    );
}
