//! Native window coverage, with explicit scale override rather than an OS-DPI claim.
use super::*;
use slint::{Timer, TimerMode};
use std::{
    io::Write,
    time::{Duration, Instant},
};

fn capture(w: &SortSizeWindow, out: &Path, name: &str) -> u64 {
    let pixels = w.window().take_snapshot().unwrap();
    let mut hash = 0xcbf29ce484222325u64;
    for p in pixels.as_slice() {
        for b in [p.r, p.g, p.b] {
            hash = (hash ^ u64::from(b)).wrapping_mul(0x100000001b3);
        }
    }
    if !name.is_empty() {
        let mut f = std::io::BufWriter::with_capacity(
            128 * 1024,
            std::fs::File::create(out.join(format!("{name}.ppm"))).unwrap(),
        );
        write!(f, "P6\n{} {}\n255\n", pixels.width(), pixels.height()).unwrap();
        for p in pixels.as_slice() {
            f.write_all(&[p.r, p.g, p.b]).unwrap();
        }
        f.flush().unwrap();
    }
    hash
}
pub fn run(out: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&out)?;
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    std::fs::create_dir(&source)?;
    for i in 0..24 {
        let folder = source.join(format!("Folder-{i:02}-{}", "long-name-".repeat(8)));
        std::fs::create_dir(&folder)?;
        std::fs::write(folder.join("contents.bin"), vec![0; 1024 * (i + 1)])?;
    }
    let (w, c) = crate::new_session(Default::default(), Some("en".into()))?;
    let h = connect(&w, &c)?;
    use sortsize_core::{MetadataValue, ValueAccuracy, volumes::*};
    let known = |value| MetadataValue::Known {
        value,
        accuracy: ValueAccuracy::Exact,
    };
    c.borrow_mut().sources.current = Some(crate::volume_view::Prepared::new(VolumeDiscovery {
        volumes: (0..8)
            .map(|i| {
                let path = temp.path().join(format!("display-only-volume-{i}"));
                Volume {
                    identity: VolumeIdentity::WindowsPath(path.clone()),
                    label: MetadataValue::Known {
                        value: format!("Volume-{i}-{}", "long-label-".repeat(8)).into(),
                        accuracy: ValueAccuracy::Exact,
                    },
                    filesystem: MetadataValue::Unknown,
                    kind: SourceKind::Local,
                    mounts: vec![VolumeMount {
                        path,
                        mount_id: None,
                        root: None,
                    }],
                    capacity: VolumeCapacity::from_bytes(
                        known(9_000_000_000_000_000),
                        known(4_000_000_000_000_000),
                        known(5_000_000_000_000_000),
                    ),
                    availability: VolumeAvailability::Available,
                }
            })
            .collect(),
        enumeration_time: Duration::ZERO,
        capacity_time: Duration::ZERO,
        capacity_calls: 0,
    }));
    c.borrow().show_sources(&w);

    w.invoke_memory_select(1);
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.set_treemap_expanded(true);
    w.set_navigation_open(true);
    let weak = w.as_weak();
    let timer = Timer::default();
    let started = Instant::now();
    let mut stage = 0;
    let mut at = Instant::now();
    let mut case = 0usize;
    let mut tab = 0;
    let mut epoch = 0;
    let mut quanta = 0;
    let mut max_tick = 0f64;
    let mut records = Vec::new();
    let mut original_tiles = Vec::new();
    let mut presentation_changes = 0;
    let mut first_mode_hash = None;
    timer.start(TimerMode::Repeated,Duration::from_millis(20),move||{
        let w=weak.upgrade().unwrap();let tick=Instant::now();c.borrow_mut().poll(&w);h.borrow_mut().poll(&w,&c.borrow());max_tick=max_tick.max(tick.elapsed().as_secs_f64()*1000.);
        assert!(started.elapsed()<Duration::from_secs(600),"L16 timeout stage {stage}");assert!(w.get_errors().is_empty(),"{}",w.get_errors());
        match stage {
            0 if started.elapsed()>Duration::from_millis(300)=>{w.invoke_start_scan();stage=1;},
            1 if h.borrow().current.as_ref().is_some_and(|p|!p.busy&&p.measure_ready)=>{
                let host=h.borrow();let p=host.current.as_ref().unwrap();epoch=host.epoch;quanta=p.quanta;
                assert_eq!(p.frame.rows.len(),24);assert_eq!(p.tiles.len(),24);
                original_tiles=p.tiles.clone();drop(host);stage=2;
            },
            2=>{
                let language=(case%8) as i32;let dense=(case/8)%2==1;let theme=((case/16)%3) as i32;let size=(case/48)%3;
                w.invoke_set_language(if language==7{1}else{language+1});
                if language==7{slint::select_bundled_translation("qps-ploc").unwrap();}
                w.invoke_set_theme(theme);w.invoke_set_density(dense);w.set_ribbon_tab(0);
                w.set_navigation_open(true);w.invoke_set_sidebar_collapsed(false);
                let scale=w.window().scale_factor();let(width,height)=[(780,520),(960,600),(1440,900)][size];
                w.window().set_size(slint::PhysicalSize::new((width as f32*scale).round()as u32,(height as f32*scale).round()as u32));
                tab=0;at=Instant::now();stage=3;
            },
            3 if at.elapsed()>Duration::from_millis(60)=>{
                let scale=w.window().scale_factor();let actual=w.window().size();
                if tab==0 {
                    assert!(w.get_scan_toolbar_bottom()<=w.get_navigation_toolbar_top()+0.5,"overlapping command rows");
                    assert!(w.get_commands_bottom()<=w.get_workspace_top()+0.5,"commands overlap workspace");
                }
                assert!(w.get_detail_viewport_height()>=34.,"no visible table row: {}",w.get_detail_viewport_height());
                let name=if case.is_multiple_of(48){format!("size-{}-tab-{tab}",case/48)}else{String::new()};
                let hash=capture(&w,&out,&name);
                records.push(format!("{{\"context\":{case},\"tab\":{tab},\"width\":{},\"height\":{},\"scale\":{scale},\"render_hash_fnv64\":\"{hash:016x}\"}}",actual.width,actual.height));
                tab+=1;if tab<5{w.set_ribbon_tab(tab);at=Instant::now();}else{
                    case+=1;if case<144{stage=2;}else{w.set_ribbon_tab(1);w.set_cache_visible(true);w.invoke_close_cache_panel();assert!(w.get_cache_trigger_focused());w.set_ribbon_tab(2);w.set_preferences_open(true);w.invoke_close_preferences();assert!(w.get_display_trigger_focused());
                        w.invoke_set_language(1);w.set_ribbon_tab(0);w.invoke_set_treemap_mode(0);at=Instant::now();stage=4;
                    }
                }
            },
            4 if at.elapsed()>Duration::from_millis(100)=>{let host=h.borrow();epoch=host.epoch;quanta=host.current.as_ref().unwrap().quanta;original_tiles=host.current.as_ref().unwrap().tiles.clone();drop(host);first_mode_hash=Some(capture(&w,&out,"mosaic"));w.invoke_set_treemap_mode(1);at=Instant::now();stage=5;},
            5 if at.elapsed()>Duration::from_millis(100)=>{
                assert_ne!(first_mode_hash,Some(capture(&w,&out,"bands")));
                stage=6;
            },
            6=>{
                let i=presentation_changes;w.set_ribbon_tab(i%5);w.invoke_set_treemap_mode(i%2);w.invoke_set_sidebar_width(if i%2==0{260.}else{340.});w.invoke_set_sidebar_collapsed(i%2==0);
                presentation_changes+=1;
                if presentation_changes%50==0 {capture(&w,&out,"");}
                if presentation_changes<1000 {return;}
                let host=h.borrow();let p=host.current.as_ref().unwrap();assert_eq!(host.epoch,epoch);assert_eq!(p.quanta,quanta);assert_eq!(p.tiles,original_tiles);
                let usage=sortsize_core::resources::Governor::global().usage();
                std::fs::write(out.join("native-l16.json"),format!("{{\"contexts\":144,\"rendered_frames\":{},\"presentation_changes\":1000,\"acquisition_quanta_unchanged\":true,\"view_epoch_unchanged\":true,\"same_projection_both_views\":true,\"max_poll_ms\":{max_tick},\"max_apply_ms\":{},\"accounted_bytes\":{},\"native_window\":true,\"scale_method\":\"SLINT_SCALE_FACTOR override\",\"os_dpi_certified\":false,\"human_interaction\":false,\"reader_certified\":false,\"frames\":[{}]}}",records.len(),host.max_apply_ms,usage.categories.iter().sum::<u64>(),records.join(","))).unwrap();
                slint::quit_event_loop().unwrap();stage=7;
            },_=>{}
        }
    });
    w.run()?;
    timer.stop();
    Ok(())
}
