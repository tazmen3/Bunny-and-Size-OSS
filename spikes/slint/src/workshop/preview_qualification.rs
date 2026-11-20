//! Pointer event to successful software-buffer submission, not poll/apply.
use super::*;
use i_slint_backend_winit::presentation_probe as probe;
use slint::{
    Model, Timer, TimerMode,
    platform::{PointerEventButton, WindowEvent},
};
use std::time::{Duration, Instant};
fn resources() -> serde_json::Value {
    let usage = sortsize_core::resources::Governor::global().usage();
    let reading = sortsize_core::resources::native_memory();
    serde_json::json!({"mode":usage.limits.requested.key(),"limit_bytes":usage.limits.bytes,"category_limits":usage.limits.categories,"accounted_categories":usage.categories,"fallback":usage.limits.fallback,"native_total":reading.map(|m|m.total),"native_available":reading.map(|m|m.available)})
}
fn click(w: &SortSizeWindow) {
    let p = slint::LogicalPosition::new(w.get_cache_open_x() + 15., w.get_cache_open_y() + 12.);
    probe::arm();
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: p,
        button: PointerEventButton::Left,
    });
    w.window().dispatch_event(WindowEvent::PointerReleased {
        position: p,
        button: PointerEventButton::Left,
    });
}
pub fn run(
    root: PathBuf,
    out: PathBuf,
    repetitions: usize,
    profile: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if repetitions > 100 {
        return Err("0 (rejection) or 1..100 repetitions".into());
    }
    let mode = MemoryMode::from_key(profile).ok_or("unknown memory profile")?;
    std::fs::create_dir_all(&out)?;
    let (w, c) = crate::new_session(Default::default(), Some("en".into()))?;
    c.borrow().show_preferences(&w);
    w.invoke_memory_select(search::MODES.iter().position(|m| *m == mode).unwrap() as i32);
    let h = connect(&w, &c)?;
    h.borrow().service.lane.lock().unwrap().cache_root = Some(root);
    w.window().set_size(slint::PhysicalSize::new(1280, 900));
    w.set_cache_visible(true);
    let weak = w.as_weak();
    let timer = Timer::default();
    let begin = Instant::now();
    let mut stage = 0;
    let mut at = Instant::now();
    let mut trial = 0;
    let mut cancelled = 0;
    let mut cancellation_settled = Vec::new();
    let mut pending_cancel = None::<Instant>;
    let mut firsts = Vec::new();
    let mut prefixes = Vec::new();
    let mut certifications = Vec::new();
    let mut heartbeat_max = 0_f64;
    let mut previous = Instant::now();
    let mut render_language = 1;
    let mut renders = Vec::new();
    let mut resource_samples = Vec::new();

    timer.start(TimerMode::Repeated,Duration::from_millis(5),move||{
        let w=weak.upgrade().unwrap();let now=Instant::now();if stage==2 { heartbeat_max=heartbeat_max.max(now.duration_since(previous).as_secs_f64()*1000.); } previous=now;
        c.borrow_mut().poll(&w);h.borrow_mut().poll(&w,&c.borrow());
        if repetitions>0 && !w.get_cache_diagnostic().is_empty() {
            let report=serde_json::json!({"status":"REFUSED","diagnostic":w.get_cache_diagnostic().as_str(),"profile":mode.key(),"stage":stage,"trial":trial,"responsiveness_trials":repetitions,"finished_certification_trials":certifications.len(),"first_useful_present_ms":firsts,"prefix_present_ms":prefixes,"certification_present_ms":certifications,"cancellation_settled_ms":cancellation_settled,"cancelled_trials":cancelled,"heartbeat_max_ms":heartbeat_max,"resources_at_refusal":resources(),"resource_samples":resource_samples,"criteria_l17_unchanged":true});
            std::fs::write(out.join("native-preview.json"),serde_json::to_vec_pretty(&report).unwrap()).unwrap();
            probe::disarm();super::qualification::capture(&w,&out,"refused");slint::quit_event_loop().unwrap();stage=7;return;
        }
        assert!(begin.elapsed()<Duration::from_secs(1200),"preview timeout stage {stage}: {} / {}",w.get_cache_status(),w.get_errors());
        if w.get_cache_token()>0 && let Some(start)=pending_cancel.take() { cancellation_settled.push(start.elapsed().as_secs_f64()*1000.); }
        match stage {
            0 if begin.elapsed()>Duration::from_millis(300)=>{w.invoke_cache_action(0,0,0);stage=1;},
            1 if w.get_cache_token()>0 && at.elapsed()>Duration::from_millis(60)=>{
                assert_eq!(h.borrow().cache_current.as_ref().unwrap().catalogue.len(),1);
                resource_samples.push(resources());
                let epoch=h.borrow().epoch;click(&w);assert!(h.borrow().epoch>epoch,"button callback not invoked");at=Instant::now();stage=2;
            },
            2=>{
                let samples=probe::samples();
                if repetitions==0 && samples.iter().any(|s|s.0==4) {
                    assert!(w.get_cache_diagnostic().contains("corrupt"),"unexpected refusal: {}",w.get_cache_diagnostic());
                    assert_eq!(w.get_preview_rows().row_count(),0);assert!(h.borrow().current.is_none());assert_eq!(w.get_details().row_count(),0);
                    let report=serde_json::json!({"profile":mode.key(),"resources":resources(),"rejected":true,"prefix_was_presented":samples.iter().any(|s|s.0==2),"native_presentations":samples,"certified_view_published":false,"rows_after_rejection":0,"diagnostic":w.get_cache_diagnostic().as_str()});
                    std::fs::write(out.join("native-refusal.json"),serde_json::to_vec_pretty(&report).unwrap()).unwrap();probe::disarm();super::qualification::capture(&w,&out,"invalidated");slint::quit_event_loop().unwrap();stage=7;return;
                }
                if let Some(first)=samples.iter().find(|s|matches!(s.0,1..=3)) && firsts.len()==trial { firsts.push(first.1);prefixes.push(None); }
                if let Some(prefix)=samples.iter().find(|s|s.0==2) {
                    if prefixes.len()>trial {prefixes[trial]=Some(prefix.1);}
                    assert!(h.borrow().current.is_none() || w.get_preview_state()==3);

                }
                if trial<repetitions && firsts.len()>trial && (samples.iter().any(|s|matches!(s.0,2|3)) || at.elapsed()>Duration::from_millis(100)) {
                    if matches!(w.get_preview_state(),1|2) { pending_cancel=Some(Instant::now());w.invoke_cancel_scan();assert_eq!(w.get_preview_state(),5);cancelled+=1; }
                    probe::disarm();trial+=1;w.invoke_cache_action(0,0,0);at=Instant::now();stage=1;
                } else if trial>=repetitions && samples.iter().any(|s|s.0==3) {
                    certifications.push(samples.iter().find(|s|s.0==3).unwrap().1);probe::disarm();trial+=1;
                    if certifications.len()==3 {

                        let quantile=|v:&[f64],q:f64| {let mut x=v.to_vec();x.sort_by(f64::total_cmp);x[((x.len()as f64*q).ceil()as usize).saturating_sub(1)]};
                        let report=serde_json::json!({"status":"COMPLETE","profile":mode.key(),"resource_samples":resource_samples,"resources_after_trials":resources(),"method":"injected native-window pointer event to successful softbuffer present_with_damage","physical_input":false,"compositor_scanout_measured":false,"os_dpi_certified":false,"reader_certified":false,"first_useful_present_ms":firsts,"prefix_present_ms":prefixes,"certification_present_ms":certifications,"p95_first_useful_ms":quantile(&firsts,0.95),"p99_first_useful_ms":quantile(&firsts,0.99),"maximum_first_useful_ms":firsts.iter().copied().fold(0.,f64::max),"heartbeat_max_ms":heartbeat_max,"cancellation_settled_ms":cancellation_settled,"cancelled_trials":cancelled,"finished_certification_trials":3,"responsiveness_trials":repetitions,"criteria_l17_unchanged":true});
                        std::fs::write(out.join("native-preview.json"),serde_json::to_vec_pretty(&report).unwrap()).unwrap();super::qualification::capture(&w,&out,"verified");w.invoke_cache_action(0,0,0);stage=4;
                    }else{w.invoke_cache_action(0,0,0);at=Instant::now();stage=1;}
                }
            },
            4 if w.get_cache_token()>0=>{ w.invoke_cache_action(2,0,w.get_cache_token());stage=5; },
            5 if matches!(w.get_preview_state(),2|3)=>{w.window().set_size(slint::PhysicalSize::new(780,520));w.invoke_set_language(render_language);at=Instant::now();stage=6;},
            6 if at.elapsed()>Duration::from_millis(100)=>{
                let state=w.get_preview_state();super::qualification::capture(&w,&out,&format!("recorded-language-{render_language}"));
                renders.push(serde_json::json!({"language":render_language,"state":state,"width":w.window().size().width,"height":w.window().size().height}));
                render_language+=1;
                if render_language<=7 {w.invoke_set_language(render_language);at=Instant::now();} else {
                    std::fs::write(out.join("recorded-renders.json"),serde_json::to_vec_pretty(&renders).unwrap()).unwrap();
                    if matches!(state,1|2) {w.invoke_cancel_scan();}slint::quit_event_loop().unwrap();stage=7;
                }
            },
            _=>{}
        }
    });
    w.run()?;
    timer.stop();
    Ok(())
}
