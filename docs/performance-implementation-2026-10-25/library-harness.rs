// Actual indexed Settings APIs used by library_ui; no GPUI layout/display claim.
#![allow(dead_code)]
#[path = "../../src/model.rs"] mod model;
#[path = "../../src/theme.rs"] mod theme;
#[path = "../../src/shortcuts.rs"] mod shortcuts;
#[path = "../../src/store.rs"] mod store;
use std::{hint::black_box, path::PathBuf, time::Instant};
use store::{Settings, LibraryVideo, RecentFile, Store};
fn measure(mut f: impl FnMut(), rounds: usize) -> serde_json::Value {
    f(); let mut xs = Vec::new();
    for _ in 0..rounds { let at=Instant::now(); f(); xs.push(at.elapsed().as_secs_f64()*1000.); }
    xs.sort_by(f64::total_cmp);
    serde_json::json!({"p50_ms":xs[xs.len()/2],"p95_ms":xs[(xs.len()*95/100).min(xs.len()-1)],"rounds":rounds})
}
fn main() -> anyhow::Result<()> {
    let folder=PathBuf::from(std::env::args_os().nth(1).expect("isolated output folder"));
    for n in [100, 1000, 5000, 20000] {
        let mut data=Settings::default();
        for i in 0..n {
            let path=PathBuf::from(format!("/generated/perf/Show{:05}.S01E{:04}.mkv",i/20,i%20));
            let title=format!("Generated Video {i:06}");
            data.library.push(LibraryVideo { path:path.clone(),title:title.clone(),duration:3600.,width:1920,height:1080,poster:Some(folder.join("poster.png")),..Default::default() });
            data.recent.push(RecentFile { path,title,position:if i%3==0{0.}else{1800.},duration:3600.,finished:i%3==0,..Default::default() });
        }
        data.invalidate();
        let mut st=Store::load_from(folder.join(format!("settings-{n}.json"))); st.data=data;
        let save=measure(||st.save().unwrap(),9);
        let queue=measure(|| { st.data.volume+=0.001; black_box(st.queue_save().unwrap()); },9); st.flush()?;
        let filter=measure(|| { black_box(st.data.sorted_library(store::Sort::Title).into_iter().filter(|v|!st.data.watched(&v.path)).count()); },9);
        let metadata=measure(|| {
            for video in &st.data.library {
                let path=&video.path;
                let duration=st.data.recent_file(path).map(|v|v.duration).filter(|v|*v>0.).or_else(||st.data.video(path).map(|v|v.duration));
                let resume=st.resume_position(path); black_box(duration); black_box(resume);
                if !(resume>0. && duration.unwrap_or(0.)>0.) { black_box(st.data.watched(path)); }
                black_box(st.data.video(path).and_then(|v|v.poster.as_ref()));
            }
        },9);
        let continuing=measure(|| {black_box(st.data.continue_watching().count());},9);
        let shows=measure(|| {black_box(st.data.shows());},9);
        println!("{}",serde_json::json!({"kind":"implemented_library_apis","n":n,"sync_save_unchanged":save,"queue_changed_settings_ui_work":queue,"sort_and_unwatched_filter":filter,"all_card_metadata_lookups_no_gpui":metadata,"continue_watching":continuing,"cached_shows":shows}));
    }
    Ok(())
}
