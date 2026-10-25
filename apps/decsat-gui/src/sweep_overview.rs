//! One-shot SDR sweep snapshot. No changes to the demodulation engine.
use std::sync::{Arc,Mutex};
use decsat_core::Iq;
use decsat_io::IqSource;
use eframe::egui;
#[derive(Default)]
pub struct Snapshot {pub values:Vec<(f64,f32)>,pub done:bool,pub progress:f32,pub error:String}
pub struct Overview {pub visible:bool,pub shared:Arc<Mutex<Snapshot>>,pub busy:bool,pub restart:bool,pub min:f64,pub max:f64,pub scope:usize,pub custom_lo:f64,pub custom_hi:f64}
impl Default for Overview {fn default()->Self{Self{visible:false,shared:Arc::new(Mutex::new(Snapshot::default())),busy:false,restart:false,min:50e6,max:6e9,scope:0,custom_lo:400.0,custom_hi:1000.0}}}
// In-place radix-2 FFT. A short FFT is sufficient for the static overview;
// overlap and edge-bin rejection reduce window seams.
fn fft(buf:&mut [(f32,f32)]){
 let n=buf.len();let mut j=0usize;
 for i in 1..n {let mut b=n>>1;while j&b!=0 {j^=b;b>>=1;}j^=b;if i<j{buf.swap(i,j);}}
 let mut len=2;while len<=n {let theta=-std::f32::consts::TAU/len as f32;
   for start in (0..n).step_by(len) {for k in 0..len/2 {let a=theta*k as f32;let (si,co)=a.sin_cos();let (re,im)=buf[start+k+len/2];let t=(re*co-im*si,re*si+im*co);let u=buf[start+k];buf[start+k]=(u.0+t.0,u.1+t.1);buf[start+k+len/2]=(u.0-t.0,u.1-t.1);}}
 len*=2;}
}
fn profile(iq:&[Iq])->Vec<f32>{
 const N:usize=512;const GROUP:usize=16;
 let mut powers=[0f64;N];let mut transforms=0;
 for chunk in iq.as_chunks::<N>().0.iter().take(8){let mut bins=vec![(0f32,0f32);N];
   for (i,z) in chunk.iter().enumerate(){let win=0.5-0.5*(std::f32::consts::TAU*i as f32/N as f32).cos();bins[i]=(z.re*win,z.im*win);}
   fft(&mut bins);for (i,b) in bins.iter().enumerate(){powers[i]+=(b.0*b.0+b.1*b.1) as f64;}transforms+=1;
 }
 if transforms==0{return Vec::new();}
 // 32 averaged bins per receiver step, rejecting 12.5% of each band edge.
 (4..28).map(|k|{let mut sum=0f64;for i in k*GROUP..(k+1)*GROUP {let idx=(i+N/2)%N;sum+=powers[idx];}(10.0*(sum/(GROUP*transforms) as f64+1e-16).log10()) as f32}).collect()
}
pub fn spawn(shared:Arc<Mutex<Snapshot>>,mut src:Box<dyn IqSource>,mut tune:Box<dyn FnMut(f64)+Send>,lo:f64,hi:f64){
 std::thread::spawn(move||{
 let fs=src.sample_rate();let step=fs*0.70;
 if !(step>0.0 && hi>lo){if let Ok(mut s)=shared.lock(){s.error="Invalid sweep range or sample rate".into();s.done=true;}return;}
 let count=(((hi-lo)/step).ceil() as usize).max(1);
 let mut out=Vec::<Iq>::with_capacity(65536);let mut points=Vec::new();
 for index in 0..count {let center=(lo+step*(index as f64+0.5)).min(hi-fs*0.15).max(lo+fs*0.15);
   tune(center);let mut failed=None;
   // Flush the samples captured around the previous tuning position.
   for _ in 0..3{out.clear();if let Err(e)=src.read(&mut out){failed=Some(e.to_string());break;}}
   if failed.is_none(){out.clear();match src.read(&mut out){Ok(_)=>{let bins=profile(&out);for (i,p) in bins.iter().enumerate(){let f=center+((i as f64+4.5)/32.0-0.5)*fs;if f>=lo&&f<=hi{points.push((f,*p));}}},Err(e)=>failed=Some(e.to_string())}}
   if let Some(e)=failed {drop(src);if let Ok(mut s)=shared.lock(){s.error=e;s.done=true;}return;}
   if let Ok(mut s)=shared.lock(){s.values=points.clone();s.progress=(index+1) as f32/count as f32;}
 }
 drop(src);
 if let Ok(mut s)=shared.lock(){s.done=true;}
 });
}
/// Optimized UHD/B210 sweep: one USRP session, deterministic stream resets,
/// readback of the actual tuned frequency, and fresh IQ per frequency step.
#[cfg(all(windows,feature="uhd"))]
pub fn spawn_uhd(shared:Arc<Mutex<Snapshot>>,mut src:decsat_io::UhdSource,lo:f64,hi:f64){
 std::thread::spawn(move||{
  let fs=src.sample_rate();let step=fs*0.70;
  if !(step>0.0 && hi>lo) {if let Ok(mut s)=shared.lock(){s.error="Invalid UHD sweep range or sample rate".into();s.done=true;}return;}
  let count=(((hi-lo)/step).ceil() as usize).max(1);
  let mut out=Vec::with_capacity(4096);let mut points=Vec::new();
  for index in 0..count {
   let center=(lo+step*(index as f64+0.5)).min(hi-fs*0.15).max(lo+fs*0.15);
   match src.sweep_capture(center,&mut out){
    Ok(actual)=>{
     let bins=profile(&out);
     for (i,p) in bins.iter().enumerate(){let f=actual+((i as f64+4.5)/32.0-0.5)*fs;if f>=lo&&f<=hi{points.push((f,*p));}}
    },
    Err(e)=>{if let Ok(mut s)=shared.lock(){s.error=e.to_string();s.done=true;}return;}
   }
   if let Ok(mut s)=shared.lock(){s.values=points.clone();s.progress=(index+1) as f32/count as f32;}
  }
  if let Ok(mut s)=shared.lock(){s.done=true;}
 });
}
pub fn draw(ui:&mut egui::Ui,overview:&mut Overview,center:f64,rate:f64)->Option<f64>{
 let (rect,response)=ui.allocate_exact_size(egui::vec2(ui.available_width(),92.0),egui::Sense::click_and_drag());let painter=ui.painter_at(rect);
 painter.rect_filled(rect,2.0,egui::Color32::from_gray(15));
 let lo=overview.min;let hi=overview.max;let span=(hi-lo).max(1.0);
 let tx=|f:f64| rect.left()+((f-lo)/span).clamp(0.0,1.0) as f32*rect.width();
 if let Ok(s)=overview.shared.lock()&& s.values.len()>1{let min_db=s.values.iter().map(|x|x.1).fold(f32::INFINITY,f32::min);let max_db=s.values.iter().map(|x|x.1).fold(f32::NEG_INFINITY,f32::max);let r=(max_db-min_db).max(15.0);
   let positions:Vec<_>=s.values.iter().map(|(f,v)|egui::pos2(tx(*f),rect.bottom()-4.0-((*v-min_db)/r).clamp(0.0,1.0)*(rect.height()-12.0))).collect();painter.add(egui::Shape::line(positions,egui::Stroke::new(1.1,egui::Color32::LIGHT_BLUE)));}
 let left=tx(center-rate*0.5);let right=tx(center+rate*0.5);
 let selection=egui::Rect::from_min_max(egui::pos2(left,rect.top()),egui::pos2(right.max(left+2.0),rect.bottom()));
 painter.rect_filled(selection,0.0,egui::Color32::from_rgba_unmultiplied(40,170,245,55));
 painter.rect_stroke(selection,0.0,egui::Stroke::new(1.5,egui::Color32::LIGHT_BLUE),egui::StrokeKind::Inside);
 painter.text(rect.left_top()+egui::vec2(5.0,4.0),egui::Align2::LEFT_TOP,format!("{:.0}–{:.0} MHz",lo/1e6,hi/1e6),egui::FontId::proportional(11.0),egui::Color32::GRAY);
 painter.text(rect.right_top()+egui::vec2(-5.0,4.0),egui::Align2::RIGHT_TOP,format!("{:.3} MHz · {:.2} MS/s",center/1e6,rate/1e6),egui::FontId::proportional(11.0),egui::Color32::WHITE);
 if (response.dragged()||response.clicked()) && let Some(p)=response.interact_pointer_pos(){return Some((lo+(p.x-rect.left()) as f64/rect.width() as f64*span).clamp(lo+rate/2.0,hi-rate/2.0));}
 None
}
