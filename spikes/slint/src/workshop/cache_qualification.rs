//! Automated recipe in a native Winit window; no physical/DPI/manual claim.
use super::*;
use slint::{Model, Timer, TimerMode};
use std::{
    io::Write,
    time::{Duration, Instant},
};
fn capture(w: &SortSizeWindow, out: &Path, name: &str) {
    let pixels = w.window().take_snapshot().unwrap();
    let mut f = std::fs::File::create(out.join(format!("{name}.ppm"))).unwrap();
    write!(f, "P6\n{} {}\n255\n", pixels.width(), pixels.height()).unwrap();
    for p in pixels.as_slice() {
        f.write_all(&[p.r, p.g, p.b]).unwrap();
    }
}
pub fn run(out: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&out)?;
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    std::fs::create_dir(&source)?;
    std::fs::write(source.join("changed"), [0; 240])?;
    for i in 0..430 {
        std::fs::write(source.join(format!("zero-{i:04}")), [])?;
    }
    let (w, c) = crate::new_session(Default::default(), Some("fr-FR".into()))?;
    c.borrow().show_preferences(&w);
    let h = connect(&w, &c)?;
    h.borrow().service.lane.lock().unwrap().cache_root = Some(temp.path().join("state"));
    w.invoke_memory_select(1);
    w.set_scan_path(c.borrow_mut().path_input.select(source.clone()).into());
    w.window().set_size(slint::PhysicalSize::new(1280, 960));
    let weak = w.as_weak();
    let timer = Timer::default();
    let begin = Instant::now();
    let mut stage = 0;
    let mut at = Instant::now();
    let mut language = 0;
    let mut first = slint::SharedString::default();
    let mut max_tick = 0_f64;
    let mut measured = None;
    let mut previous = 0;
    timer.start(TimerMode::Repeated,Duration::from_millis(10),move|| {
        let w=weak.upgrade().unwrap();let tick=Instant::now();c.borrow_mut().poll(&w);h.borrow_mut().poll(&w,&c.borrow());max_tick=max_tick.max(tick.elapsed().as_secs_f64()*1000.);
        assert!(begin.elapsed()<Duration::from_secs(120),"cache native timeout stage {stage}: {} / {}",w.get_errors(),w.get_cache_status());assert!(w.get_errors().is_empty(),"{}",w.get_errors());
        match stage {
            0 if begin.elapsed()>Duration::from_millis(300)=>{w.invoke_start_scan();stage=1;},
            1 if h.borrow().current.as_ref().is_some_and(|p|!p.busy && p.measure_ready)=>{assert!(w.get_summary().contains("240"));w.set_cache_visible(true);w.invoke_cache_action(0,0,0);stage=2;},
            2 if h.borrow().cache_current.is_some()=>{w.invoke_cache_action(1,0,w.get_cache_token());stage=3;},
            3 if h.borrow().cache_current.as_ref().is_some_and(|p|p.id==h.borrow().cache_id && p.catalogue.len()==1)=>{measured=h.borrow().cache_current.as_ref().unwrap().catalogue[0].measured;let m=std::fs::metadata(&source).unwrap().modified().unwrap();std::fs::write(source.join("changed"),[0;160]).unwrap();assert_eq!(m,std::fs::metadata(&source).unwrap().modified().unwrap());w.invoke_cache_action(3,0,w.get_cache_token());stage=4;},
            4 if h.borrow().current.as_ref().is_some_and(|p|!p.busy && p.measure_ready)=>{assert!(w.get_summary().contains("160"));w.invoke_cache_action(0,0,0);stage=5;},
            5 if h.borrow().cache_current.as_ref().is_some_and(|p|p.id==h.borrow().cache_id && p.epoch==h.borrow().epoch && p.catalogue.len()==2)=>{previous=h.borrow().cache_current.as_ref().unwrap().catalogue.iter().position(|s|s.context.generation==GenerationId(1)).unwrap() as i32;w.invoke_cache_action(5,previous,w.get_cache_token());stage=6;},
            6 if w.get_diff_rows().row_count()==200=>{first=w.get_diff_rows().row_data(0).unwrap().old_path;w.invoke_cache_page(true);stage=7;},
            7 if w.get_diff_rows().row_data(0).is_some_and(|r|r.old_path!=first)=>{w.invoke_cache_page(false);stage=8;},
            8 if w.get_diff_rows().row_data(0).is_some_and(|r|r.old_path==first)=>{w.invoke_set_language(language+1);at=Instant::now();stage=9;},
            9 if at.elapsed()>Duration::from_millis(350) && h.borrow().cache_current.as_ref().is_some_and(|p|p.presentation==c.borrow().presentation)=>{
                assert!(w.get_diff_summary().contains("240") && w.get_diff_summary().contains("160"));assert_eq!(w.get_diff_rows().row_data(0).unwrap().old_path,first);capture(&w,&out,&format!("diff-language-{language}"));language+=1;if language<7{w.invoke_set_language(language+1);at=Instant::now();}else{w.window().set_size(slint::PhysicalSize::new(760,800));at=Instant::now();stage=10;}
            },
            10 if at.elapsed()>Duration::from_millis(300)=>{capture(&w,&out,"diff-compact");w.invoke_cache_action(2,previous,w.get_cache_token());stage=11;},
            11 if h.borrow().current.as_ref().is_some_and(|p|!p.busy && p.quanta==0 && p.measure_ready)=>{assert!(w.get_summary().contains("240"));std::fs::rename(&source,temp.path().join("source-offline")).unwrap();w.invoke_show_analysis();w.set_search_name("zero-".into());w.invoke_search_submit();stage=12;},
            12 if h.borrow().search_current.as_ref().is_some_and(|p|!p.busy && p.matches==430)=>{assert_eq!(h.borrow().current.as_ref().unwrap().quanta,0);assert_eq!(h.borrow().cache_current.as_ref().unwrap().catalogue.iter().find(|s|s.context.generation==GenerationId(1)).unwrap().measured,measured);at=Instant::now();stage=13;},
            13 if at.elapsed()>Duration::from_millis(250)=>{
                capture(&w,&out,"offline-search");let report=format!("{{\"native_rendered_window\":true,\"real_files\":431,\"old_logical_bytes\":240,\"new_logical_bytes\":160,\"parent_mtime_unchanged\":true,\"offline_root_renamed\":true,\"offline_query_matches\":430,\"offline_acquisition_quanta\":0,\"languages\":7,\"pagination_next_previous\":true,\"max_ui_poll_ms\":{max_tick},\"max_apply_ms\":{},\"width\":{},\"height\":{},\"human_interaction\":false}}",h.borrow().max_apply_ms,w.window().size().width,w.window().size().height);std::fs::write(out.join("native-cache.json"),report).unwrap();slint::quit_event_loop().unwrap();
            },_=>{}
        }
    });
    w.run()?;
    timer.stop();
    Ok(())
}
