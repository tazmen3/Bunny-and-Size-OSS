//! Native rendered-window lab. Caller selects a real read-only tree, output is separate.
use super::*;
use slint::{
    Model, Timer, TimerMode,
    platform::{PointerEventButton, WindowEvent},
};
use std::{
    io::Write,
    time::{Duration, Instant},
};
pub(super) fn capture(w: &SortSizeWindow, out: &Path, name: &str) {
    let pixels = w.window().take_snapshot().unwrap();
    let mut f =
        std::io::BufWriter::new(std::fs::File::create(out.join(format!("{name}.ppm"))).unwrap());
    write!(f, "P6\n{} {}\n255\n", pixels.width(), pixels.height()).unwrap();
    for p in pixels.as_slice() {
        f.write_all(&[p.r, p.g, p.b]).unwrap();
    }
}
fn oracle(path: &Path) -> std::io::Result<(u64, u64)> {
    let mut entries = 1;
    let mut bytes = 0;
    for item in std::fs::read_dir(path)? {
        let item = item?;
        let m = item.path().symlink_metadata()?;
        if m.is_dir() {
            let (n, b) = oracle(&item.path())?;
            entries += n;
            bytes += b;
        } else {
            entries += 1;
            if m.is_file() {
                bytes += m.len();
            }
        }
    }
    Ok((entries, bytes))
}
pub fn run(root: PathBuf, out: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&out)?;
    let (expected_entries, expected_bytes) = oracle(&root)?;
    let (w, c) = crate::new_session(Default::default(), Some("fr-FR".into()))?;
    c.borrow().show_preferences(&w);
    let h = connect(&w, &c)?;
    use sortsize_core::{MetadataValue, volumes::*};
    let volume = Volume {
        identity: VolumeIdentity::WindowsGuid(root.as_os_str().to_owned()),
        label: MetadataValue::Unknown,
        filesystem: MetadataValue::Unknown,
        kind: SourceKind::Local,
        mounts: vec![VolumeMount {
            path: root.clone(),
            mount_id: None,
            root: None,
        }],
        capacity: VolumeCapacity::from_bytes(
            MetadataValue::Unknown,
            MetadataValue::Unknown,
            MetadataValue::Unknown,
        ),
        availability: VolumeAvailability::Available,
    };
    // Inject only the discovery descriptor; the normal card preflight and native Guard run.
    let v = volume.clone();
    let discover = Arc::new(move |_: DiscoveryOptions| {
        Ok(VolumeDiscovery {
            volumes: vec![v.clone()],
            enumeration_time: Duration::ZERO,
            capacity_time: Duration::ZERO,
            capacity_calls: 0,
        })
    });
    c.borrow_mut().sources = crate::volume_view::Sources::with_provider(discover);
    c.borrow_mut().sources.current = Some(crate::volume_view::Prepared::new(VolumeDiscovery {
        volumes: vec![volume],
        enumeration_time: Duration::ZERO,
        capacity_time: Duration::ZERO,
        capacity_calls: 0,
    }));
    w.set_storage_enabled(true);
    c.borrow().show_sources(&w);
    w.window().set_size(slint::PhysicalSize::new(1280, 900));
    let weak = w.as_weak();
    let timer = Timer::default();
    let begin = Instant::now();
    let mut stage = 0;
    let mut epoch = 0;
    let mut quanta = 0;

    let mut samples = Vec::new();
    let mut acknowledgements = Vec::new();
    let mut request_at = Instant::now();
    let mut iterations = 0;
    let mut first_useful = None;
    let mut first_useful_after_click = None;
    let mut scan_start = Instant::now();
    let mut sealed_ms = 0.;
    let mut last_applied = 0;
    let mut first_path = String::new();
    let mut folder_index = 0;
    timer.start(TimerMode::Repeated,Duration::from_millis(10),move||{
        let w=weak.upgrade().unwrap();h.borrow_mut().poll(&w,&c.borrow());
        assert!(begin.elapsed()<Duration::from_secs(120),"qualification timeout, stage {stage}, errors {}",w.get_errors());
        assert!(w.get_errors().is_empty(),"{}",w.get_errors());
        if h.borrow().applied!=last_applied {last_applied=h.borrow().applied;samples.push(h.borrow().last_apply_ms);if first_useful.is_none() && w.get_details().row_count()>0 {first_useful=Some(begin.elapsed().as_secs_f64()*1000.);first_useful_after_click=Some(scan_start.elapsed().as_secs_f64()*1000.);}}
        match stage {
            0 if begin.elapsed()>Duration::from_millis(600)=>{
                let position=slint::LogicalPosition::new(w.get_source_click_x(),w.get_source_click_y());
                w.window().dispatch_event(WindowEvent::PointerPressed{position,button:PointerEventButton::Left});
                w.window().dispatch_event(WindowEvent::PointerReleased{position,button:PointerEventButton::Left});
                scan_start=Instant::now();request_at=scan_start;stage=1;
            },
            1 if h.borrow().current.as_ref().is_some_and(|p|!p.busy&&p.measure_ready)=>{
                assert!(!w.get_busy());
                sealed_ms=scan_start.elapsed().as_secs_f64()*1000.;
                request_at=Instant::now();stage=5;
            },
            5 if request_at.elapsed()>Duration::from_millis(150)=>{
                let host=h.borrow();let p=host.current.as_ref().unwrap();quanta=p.quanta;
                assert_eq!(p.summary,crate::units::Units::Binary.format(expected_bytes));
                let folder=p.frame.rows.iter().position(|r|r.kind==sortsize_store::inventory::Kind::Directory);
                assert!(folder.is_some(),"choose a real tree containing directories");
                first_path=p.path.clone();
                capture(&w,&out,"explorer");drop(host);
                folder_index=folder.unwrap() as i32;
                w.set_detail_scroll_y(-(folder_index as f32)*34.);
                request_at=Instant::now();stage=6;
            },
            6 if request_at.elapsed()>Duration::from_millis(150)=>{
                let position=slint::LogicalPosition::new(w.get_detail_click_x(),w.get_detail_click_y()+folder_index as f32*34.+w.get_detail_scroll_y());
                w.window().dispatch_event(WindowEvent::PointerPressed{position,button:PointerEventButton::Left});
                w.window().dispatch_event(WindowEvent::PointerReleased{position,button:PointerEventButton::Left});
                assert_eq!(w.get_workshop_selected(),folder_index);
                assert_eq!(w.get_current_path(),first_path); // single click never enters
                w.window().dispatch_event(WindowEvent::KeyPressed{text:"\n".into()});
                w.window().dispatch_event(WindowEvent::KeyReleased{text:"\n".into()});
                epoch=h.borrow().epoch;request_at=Instant::now();stage=2;
            },
            2 if h.borrow().current.as_ref().is_some_and(|p|p.epoch==epoch)=>{
                acknowledgements.push(request_at.elapsed().as_secs_f64()*1000.);
                assert_ne!(w.get_current_path(),first_path);assert!(w.get_has_parent());
                w.invoke_go_parent();epoch=h.borrow().epoch;request_at=Instant::now();stage=3;
            },
            3 if h.borrow().current.as_ref().is_some_and(|p|p.epoch==epoch)=>{
                acknowledgements.push(request_at.elapsed().as_secs_f64()*1000.);
                assert_eq!(w.get_current_path(),first_path);assert_eq!(h.borrow().current.as_ref().unwrap().quanta,quanta);
                if iterations==0 {iterations=1;w.set_detail_scroll_y(-(folder_index as f32)*34.);request_at=Instant::now();stage=8;}
                else if iterations<50 {iterations+=1;h.borrow_mut().navigate(Navigate::Refresh);epoch=h.borrow().epoch;request_at=Instant::now();}
                else {w.set_top_visible(true);if w.get_top_rows().row_count()>0{w.invoke_select_top(0);}stage=4;request_at=Instant::now();}
            },
            8 if request_at.elapsed()>Duration::from_millis(600)=>{
                let position=slint::LogicalPosition::new(w.get_detail_click_x(),w.get_detail_click_y()+folder_index as f32*34.+w.get_detail_scroll_y());
                for _ in 0..2 {
                    w.window().dispatch_event(WindowEvent::PointerPressed{position,button:PointerEventButton::Left});
                    w.window().dispatch_event(WindowEvent::PointerReleased{position,button:PointerEventButton::Left});
                }
                epoch=h.borrow().epoch;request_at=Instant::now();stage=2;
            },
            4 if request_at.elapsed()>Duration::from_millis(150)=>{
                capture(&w,&out,"top100");samples.sort_by(f64::total_cmp);acknowledgements.sort_by(f64::total_cmp);
                let report=format!("{{\"native_window\":true,\"real_tree\":true,\"discovery_descriptor\":\"injected local descriptor, native Guard and acquisition\",\"card_pointer_at_lower_area\":true,\"navigation_via_slint_callbacks\":true,\"oracle_entries\":{expected_entries},\"oracle_logical_bytes\":{expected_bytes},\"first_useful_ms_from_window_open\":{},\"scan_and_folder_totals_ms\":{sealed_ms},\"navigation_p95_ms\":{},\"apply_release_p95_ms\":{},\"apply_release_max_ms\":{},\"samples\":{},\"navigation_samples\":{},\"extra_acquisition_quanta\":0}}",first_useful.unwrap(),acknowledgements[acknowledgements.len()*95/100],samples[samples.len()*95/100],samples.last().unwrap(),samples.len(),acknowledgements.len());
                let report=report.replacen("{", &format!("{{\"native_single_click_enter_double_click\":true,\"first_useful_ms_after_card_click\":{},",first_useful_after_click.unwrap()),1);
                std::fs::write(out.join("native.json"),report).unwrap();slint::quit_event_loop().unwrap();
            },_=>{},
        }
    });
    w.run()?;
    timer.stop();
    Ok(())
}
