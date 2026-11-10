//! FFmpeg discovery, the quality ladder, and transcode jobs.
//!
//! Owned by the transcode agent. Intended API:
//!
//! ```ignore
//! pub struct Encoder { pub ffmpeg: PathBuf, pub ffprobe: PathBuf, pub video_encoder: String }
//! /// Finds ffmpeg/ffprobe (explicit path, PATH, then common locations such as
//! /// ~/.local/bin, /opt/homebrew/bin, /usr/local/bin) and picks
//! /// h264_videotoolbox, h264_nvenc, h264_qsv, or libx264 from `ffmpeg -encoders`.
//! pub async fn probe(explicit: Option<&Path>) -> Option<Encoder>;
//! pub struct MediaInfo { pub duration: f64, pub width: u32, pub height: u32, pub hdr: bool,
//!                        pub subtitles: Vec<protocol::EmbeddedSubtitle> }
//! pub async fn inspect(encoder: &Encoder, path: &Path) -> anyhow::Result<MediaInfo>;
//! /// The ffmpeg arguments for one job starting at `start_segment`, with
//! /// key frames forced every HLS_SEGMENT_SECONDS, scaled to the tier's
//! /// height, HDR tone-mapped to SDR, AAC stereo, fMP4 segments written to
//! /// `out_dir` as `init.mp4` and `{n}.m4s` numbered from `start_segment`.
//! pub fn job_args(encoder: &Encoder, source: &Path, quality: Quality, start_segment: u32, out_dir: &Path) -> Vec<OsString>;
//! pub async fn extract_webvtt(encoder: &Encoder, source: &Path, track: u32) -> anyhow::Result<Vec<u8>>;
//! ```
//!
//! Tests drive a generated stand-in `ffmpeg` script, as the yt-dlp tests do
//! (`src/fetch/ytdlp.rs`); real FFmpeg is only used by an `#[ignore]` test.
