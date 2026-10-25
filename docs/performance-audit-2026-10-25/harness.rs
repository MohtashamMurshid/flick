// Audit-only benchmark; not linked into Flick. See reproduce.py.

#![allow(dead_code)]
#[path = "../../src/model.rs"] mod model;
#[path = "../../src/theme.rs"] mod theme;
#[path = "../../src/shortcuts.rs"] mod shortcuts;
#[path = "../../src/store.rs"] mod store;
#[path = "../../src/playback.rs"] mod playback;
use std::{hint::black_box, path::PathBuf, time::{Instant,Duration}};
use store::{Settings,LibraryVideo,RecentFile,Store};
fn measure(mut f:impl FnMut(), rounds:usize)->serde_json::Value {
 f(); let mut xs=Vec::new(); for _ in 0..rounds {let t=Instant::now(); f(); xs.push(t.elapsed().as_secs_f64()*1000.);} xs.sort_by(f64::total_cmp);
 serde_json::json!({"p50_ms":xs[xs.len()/2],"p95_ms":xs[(xs.len()*95/100).min(xs.len()-1)],"min_ms":xs[0],"max_ms":xs[xs.len()-1],"rounds":rounds})
}
fn main()->anyhow::Result<()> {
 let folder=PathBuf::from(std::env::args_os().nth(1).unwrap());
 for n in [100,1000] {
  let mut data=Settings::default();
  for i in 0..n {let path=PathBuf::from(format!("/generated/perf/Show{:05}.S01E{:04}.mkv",i/20,i%20)); let title=format!("Generated Video {i:06}");
   data.library.push(LibraryVideo{path:path.clone(),title:title.clone(),duration:3600.,width:1920,height:1080,poster:Some(folder.join("poster.png")),..Default::default()});
   data.recent.push(RecentFile{path,title,position:if i%3==0{0.}else{1800.},duration:3600.,finished:i%3==0,..Default::default()});
  }
  let file=folder.join(format!("settings-{n}.json"));
  let mut st=Store::load_from(&file); st.data=data;
  let save=measure(||st.save().unwrap(),9);
  let size=std::fs::metadata(&file)?.len();
  let filter=measure(||{black_box(st.data.sorted_library(store::Sort::Title).into_iter().filter(|v|!st.data.watched(&v.path)).count());},9);
  let card_lookup=measure(||{
   for video in &st.data.library {
    let path=&video.path;
    let duration=st.data.recent.iter().find(|v|v.path==*path).map(|v|v.duration).filter(|v|*v>0.).or_else(||st.data.library.iter().find(|v|v.path==*path).map(|v|v.duration));
    let resume=st.resume_position(path);black_box(duration);black_box(resume);
    if !(resume>0. && duration.unwrap_or(0.)>0.) {black_box(st.data.watched(path));}
    black_box(st.data.library.iter().find(|v|v.path==*path).and_then(|v|v.poster.as_ref()));
   }
  },5);
  let continuing=measure(||{black_box(st.data.continue_watching().count());},5);
  let library_index=st.data.library.iter().map(|v|(v.path.as_path(),v)).collect::<std::collections::HashMap<_,_>>();
  let recent_index=st.data.recent.iter().map(|v|(v.path.as_path(),v)).collect::<std::collections::HashMap<_,_>>();
  for video in &st.data.library {let p=video.path.as_path();let r=recent_index.get(p);
   assert_eq!(st.resume_position(p),r.filter(|v|v.position.is_finite()&&v.duration.is_finite()&&v.position>5.&&v.duration-v.position>=10.).map_or(0.,|v|v.position));
   assert_eq!(st.data.watched(p),r.is_some_and(|v|v.finished));
   assert_eq!(video.poster.as_ref(),library_index[p].poster.as_ref());
  }
  let indexed=measure(||{
   let library=st.data.library.iter().map(|v|(v.path.as_path(),v)).collect::<std::collections::HashMap<_,_>>();
   let history=st.data.recent.iter().map(|v|(v.path.as_path(),v)).collect::<std::collections::HashMap<_,_>>();
   for video in &st.data.library {let path=video.path.as_path();let recent=history.get(path);let entry=library.get(path).unwrap();
    let duration=recent.map(|v|v.duration).filter(|v|*v>0.).unwrap_or(entry.duration);
    let resume=recent.filter(|v|v.position.is_finite()&&v.duration.is_finite()&&v.position>5.&&v.duration-v.position>=10.).map_or(0.,|v|v.position);
    black_box(duration);black_box(resume);
    if !(resume>0. && duration>0.) {black_box(recent.is_some_and(|v|v.finished));}
    black_box(entry.poster.as_ref());
   }
  },5);
  let shows=measure(||{black_box(flick::library::group_shows(st.data.library.iter().map(|v|v.path.as_path())));},9);
  println!("{}",serde_json::json!({"kind":"library_source_microbenchmark","n":n,"settings_bytes":size,"save_including_fsync":save,"sort_and_unwatched_filter":filter,"all_card_metadata_lookups_no_gpui":card_lookup,"group_shows":shows,"continue_watching":continuing,"prototype_indexed_metadata_including_index_build":indexed}));
 }
 let path=folder.join("deleted.mp4");std::fs::write(&path,b"generated placeholder")?;
 let settings=folder.join("deletion-settings.json");let mut st=Store::load_from(&settings);st.data.add_video(&path);st.remember(&path,20.,120.);st.save()?;std::fs::remove_file(&path)?;
 let mut st=Store::load_from(&settings);
 let count=st.data.library.len();st.data.library.clear();st.save()?;let loaded=Store::load_from(&settings);
 println!("{}",serde_json::json!({"kind":"external_deletion_reproduction","file_exists":path.exists(),"library_count_after_reload":count,"library_count_after_clearing_library_only_and_saving":loaded.data.library.len(),"history_count_after_reload":st.data.recent.len()}));
 let video=folder.join("fixture.mp4");
 if video.exists() {
  let t=Instant::now(); let mut p=playback::Player::new()?;let init=t.elapsed();
  let mut frames=p.render_in_background()?;
  p.load(video.to_str().unwrap())?;let t=Instant::now();let mut loaded=false;let deadline=t+Duration::from_secs(15);
  while !loaded {for e in p.poll_events(){if matches!(e,playback::PlayerEvent::FileLoaded){loaded=true;}}anyhow::ensure!(Instant::now()<deadline,"load deadline");std::thread::sleep(Duration::from_millis(2));}
  p.set_render_target(1920,1080);
  let mut first=false;while !first {while let Ok((_,f))=frames.try_recv(){if f.is_ok(){first=true;}}anyhow::ensure!(Instant::now()<deadline,"frame deadline");std::thread::sleep(Duration::from_millis(2));}
  let first=t.elapsed();
  let snap=measure(||{black_box(p.snapshot());},100);
  let pause=measure(||{p.set_property("pause","yes").unwrap();black_box(p.snapshot());p.set_property("pause","no").unwrap();black_box(p.snapshot());while frames.try_recv().is_ok(){}},40);
  p.set_property("pause","yes")?;p.set_render_target(0,0);
  let paused=measure(||{black_box(p.snapshot());},100);
  println!("{}",serde_json::json!({"kind":"native_backend","player_init_ms":init.as_secs_f64()*1000.,"load_to_first_frame_received_ms":first.as_secs_f64()*1000.,"snapshot_playing":snap,"two_pause_sets_and_two_snapshots":pause,"snapshot_paused":paused,"hwdec":p.hardware_decoder()}));
 }
 Ok(())
}
