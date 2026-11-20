//! Rendered native-window qualification. Real source reads and query timings are separate.
use super::*;
use slint::{
    Model, Timer, TimerMode,
    platform::{PointerEventButton, WindowEvent},
};
use std::io::Write;
use std::time::{Duration, Instant};
fn capture(w: &SortSizeWindow, out: &Path, name: &str) {
    let pixels = w.window().take_snapshot().unwrap();
    let mut f = std::fs::File::create(out.join(format!("{name}.ppm"))).unwrap();
    write!(f, "P6\n{} {}\n255\n", pixels.width(), pixels.height()).unwrap();
    for p in pixels.as_slice() {
        f.write_all(&[p.r, p.g, p.b]).unwrap();
    }
}
fn oracle(root: &Path) -> std::io::Result<u64> {
    let mut count = 1;
    for e in std::fs::read_dir(root)? {
        let e = e?;
        count += if e.path().symlink_metadata()?.is_dir() {
            oracle(&e.path())?
        } else {
            1
        };
    }
    Ok(count)
}
fn click(w: &SortSizeWindow, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    w.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}
fn key(w: &SortSizeWindow, text: slint::SharedString) {
    w.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    w.window().dispatch_event(WindowEvent::KeyReleased { text });
}
pub fn run(root: PathBuf, out: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&out)?;
    let expected = oracle(&root)?;
    let (w, c) = crate::new_session(Default::default(), Some("fr-FR".into()))?;
    c.borrow().show_preferences(&w);
    let h = connect(&w, &c)?;
    // Fixed explicit profile for reproducible latency; Auto refusals are recorded separately.
    w.invoke_memory_select(1);
    w.set_scan_path(c.borrow_mut().path_input.select(root).into());
    w.window().set_size(slint::PhysicalSize::new(1280, 900));
    let weak = w.as_weak();
    let timer = Timer::default();
    let begin = Instant::now();
    let mut stage = 0;
    let mut at = Instant::now();
    let mut query_start = Instant::now();
    let mut first = None;
    let mut full = 0.;
    let mut scan_ms = 0.;
    let mut quanta = 0;
    let mut language = 1;
    let mut token = 0;
    let mut snapshots = Vec::new();
    let mut max_tick = 0_f64;
    timer.start(TimerMode::Repeated,Duration::from_millis(10),move||{
        let w=weak.upgrade().unwrap();let tick=Instant::now();c.borrow_mut().poll(&w);h.borrow_mut().poll(&w,&c.borrow());max_tick=max_tick.max(tick.elapsed().as_secs_f64()*1000.);
        assert!(begin.elapsed()<Duration::from_secs(180),"search qualification timeout stage {stage}: {} / {}",w.get_errors(),w.get_search_status());assert!(w.get_errors().is_empty(),"{}",w.get_errors());
        if stage==3 && first.is_none() && h.borrow().search_current.as_ref().is_some_and(|p|p.rows.iter().any(|r|r.kind==sortsize_store::inventory::Kind::File)) {first=Some(query_start.elapsed().as_secs_f64()*1000.);}
        match stage {
            0 if begin.elapsed()>Duration::from_millis(400)=>{w.invoke_start_scan();at=Instant::now();stage=1;},
            1 if h.borrow().current.as_ref().is_some_and(|p|!p.busy&&p.measure_ready)=>{scan_ms=at.elapsed().as_secs_f64()*1000.;quanta=h.borrow().current.as_ref().unwrap().quanta;w.invoke_show_analysis();at=Instant::now();stage=2;},
            2 if at.elapsed()>Duration::from_millis(200)=>{click(&w,w.get_search_input_x()+20.,w.get_search_input_y()+10.);query_start=Instant::now();key(&w,"\n".into());stage=3;},
            3 if h.borrow().search_current.as_ref().is_some_and(|p|!p.busy)=>{full=query_start.elapsed().as_secs_f64()*1000.;assert_eq!(w.get_search_matches(),expected.to_string());assert!(w.get_search_complete());click(&w,w.get_search_stats_x()+15.,w.get_search_stats_y()+10.);assert!(w.get_search_statistics_visible());stage=4;},
            4 if !w.get_search_busy()&&!w.get_search_statistics().is_empty()=>{w.invoke_set_language(language);w.invoke_memory_select((language-1)%6);token=w.get_search_token();at=Instant::now();stage=5;},
            5 if w.get_search_token()!=token&&at.elapsed()>Duration::from_millis(250)=>{
                {let _scope=c.borrow().presentation.enter();assert_eq!(w.get_status(),tr!("Complete"));}
                assert_eq!(w.get_memory_choice(),(language-1)%6);assert_eq!(w.get_search_matches(),expected.to_string());assert_eq!(h.borrow().current.as_ref().unwrap().quanta,quanta);assert!(w.get_search_scope().contains("UTC"));
                capture(&w,&out,&format!("search-language-{language}"));snapshots.push(format!("{{\"language_choice\":{language},\"width\":{},\"height\":{},\"results\":{}}}",w.window().size().width,w.window().size().height,w.get_search_results().row_count()));
                if language<7 {language+=1;stage=4;}else{w.window().set_size(slint::PhysicalSize::new(780,520));at=Instant::now();stage=8;}
            },
            8 if at.elapsed()>Duration::from_millis(250)=>{capture(&w,&out,"statistics-minimum");click(&w,w.get_search_stats_x()+15.,w.get_search_stats_y()+10.);assert!(!w.get_search_statistics_visible());at=Instant::now();stage=6;},
            6 if at.elapsed()>Duration::from_millis(250)=>{capture(&w,&out,"search-minimum");click(&w,w.get_search_results_x()+20.,w.get_search_results_y()+15.);key(&w,slint::platform::Key::End.into());assert_eq!(w.get_search_selected(),w.get_search_results().row_count() as i32-1);key(&w,"\n".into());assert!(!w.get_search_visible());stage=7;},
            7 if h.borrow().current.as_ref().is_some_and(|p|p.epoch==h.borrow().epoch)=>{
                assert_eq!(h.borrow().current.as_ref().unwrap().quanta,quanta);
                w.invoke_show_analysis();w.invoke_search_submit();stage=9;
            },
            9 if !w.get_search_busy()&&w.get_search_token()>0=>{
                token=w.get_search_token();w.invoke_search_sort(token,2);stage=10;
            },
            10 if !w.get_search_busy()&&w.get_search_token()!=token=>{
                assert_eq!(w.get_search_order(),2);capture(&w,&out,"search-size-down-minimum");token=w.get_search_token();w.invoke_search_sort(token,1);stage=11;
            },
            11 if !w.get_search_busy()&&w.get_search_token()!=token=>{
                assert_eq!(w.get_search_order(),1);capture(&w,&out,"search-size-up-minimum");click(&w,w.get_search_duplicates_x()+20.,w.get_search_duplicates_y()+10.);assert!(w.get_search_busy());stage=12;
            },
            12 if !w.get_search_busy()&&w.get_search_duplicates_visible()=>{
                assert_eq!(h.borrow().current.as_ref().unwrap().quanta,quanta);
                capture(&w,&out,"duplicates-minimum");
                let duplicate_matches=w.get_search_matches();let duplicate_complete=w.get_search_complete();let groups=h.borrow().search_current.as_ref().unwrap().duplicate_groups.unwrap();
                let report=format!("{{\"real_tree\":true,\"initial_memory_choice\":50000,\"native_rendered_window\":true,\"oracle_warms_metadata\":true,\"oracle_entries\":{expected},\"scan_ms\":{scan_ms},\"first_result_ms\":{},\"query_complete_ms\":{full},\"max_ui_poll_ms\":{max_tick},\"keyboard_enter_search_end_select_enter_reveal\":true,\"extra_acquisition_quanta\":0,\"languages\":[{}],\"minimum_width\":{},\"minimum_height\":{}}}",first.unwrap(),snapshots.join(","),w.window().size().width,w.window().size().height);
                let report=format!("{},\"size_sorts\":[\"largest\",\"smallest\"],\"duplicate_matches\":{},\"duplicate_groups\":{groups},\"duplicates_complete\":{duplicate_complete},\"duplicate_content_action_pointer\":true}}",report.strip_suffix('}').unwrap(),duplicate_matches.parse::<u64>().unwrap());
                std::fs::write(out.join("native-search.json"),report).unwrap();slint::quit_event_loop().unwrap();
            },_=>{}
        }
    });
    w.run()?;
    timer.stop();
    Ok(())
}
