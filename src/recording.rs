use crate::blf;
use crate::can::CanFrame;
use chrono::Local;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const FRAME_QUEUE_CAPACITY: usize = 256;
const CONTROL_QUEUE_CAPACITY: usize = 16;
const EVENT_QUEUE_CAPACITY: usize = 32;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    Asc,
    Blf,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Asc => "ASC",
            Self::Blf => "BLF",
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Started {
        path: PathBuf,
        format: Format,
    },
    Stopped {
        path: PathBuf,
        format: Format,
        frames: u64,
    },
    Failed(String),
}

enum Control {
    Start { path: PathBuf, format: Format },
    Stop,
    Shutdown,
}

enum ActiveWriter {
    Text {
        writer: std::io::BufWriter<std::fs::File>,
        format: Format,
    },
    Blf(blf::StreamWriter),
}

struct ActiveRecording {
    path: PathBuf,
    format: Format,
    writer: ActiveWriter,
    frames: u64,
    last_checkpoint: Instant,
}

impl ActiveRecording {
    fn create(path: PathBuf, format: Format) -> Result<Self, String> {
        let writer = match format {
            Format::Blf => ActiveWriter::Blf(blf::StreamWriter::create(&path)?),
            Format::Csv | Format::Asc => {
                let file = std::fs::File::create(&path)
                    .map_err(|error| format!("创建记录文件失败: {error}"))?;
                let mut writer = std::io::BufWriter::new(file);
                match format {
                    Format::Csv => writeln!(writer, "Time,Ch,Dir,ID,Len,Data"),
                    Format::Asc => writeln!(
                        writer,
                        "date {}",
                        Local::now().format("%a %b %e %I:%M:%S %P %Y")
                    )
                    .and_then(|_| writeln!(writer, "base hex  timestamps absolute"))
                    .and_then(|_| writeln!(writer, "no internal events logged")),
                    Format::Blf => unreachable!(),
                }
                .map_err(|error| format!("写入记录文件头失败: {error}"))?;
                writer
                    .flush()
                    .map_err(|error| format!("刷新记录文件头失败: {error}"))?;
                ActiveWriter::Text { writer, format }
            }
        };
        Ok(Self {
            path,
            format,
            writer,
            frames: 0,
            last_checkpoint: Instant::now(),
        })
    }

    fn write_batch(&mut self, frames: &[CanFrame]) -> Result<(), String> {
        for frame in frames {
            match &mut self.writer {
                ActiveWriter::Blf(writer) => writer.push(frame)?,
                ActiveWriter::Text { writer, format } => match format {
                    Format::Csv => writeln!(
                        writer,
                        "{:.6},CAN{},{},0x{:X},{},{}",
                        frame.t,
                        frame.ch,
                        if frame.tx { "Tx" } else { "Rx" },
                        frame.id,
                        frame.data.len(),
                        frame.data_hex()
                    ),
                    Format::Asc => {
                        let id = if frame.ext {
                            format!("{:X}x", frame.id)
                        } else {
                            format!("{:X}", frame.id)
                        };
                        writeln!(
                            writer,
                            "{:.6} {} {:<16}{}   d {} {}",
                            frame.t,
                            frame.ch,
                            id,
                            if frame.tx { "Tx" } else { "Rx" },
                            frame.data.len(),
                            frame.data_hex()
                        )
                    }
                    Format::Blf => unreachable!(),
                }
                .map_err(|error| format!("写入记录文件失败: {error}"))?,
            }
            self.frames = self.frames.saturating_add(1);
        }
        if self.last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL {
            self.checkpoint()?;
        }
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<(), String> {
        match &mut self.writer {
            ActiveWriter::Text { writer, .. } => writer
                .flush()
                .map_err(|error| format!("刷新记录文件失败: {error}"))?,
            ActiveWriter::Blf(writer) => writer.flush_checkpoint()?,
        }
        self.last_checkpoint = Instant::now();
        Ok(())
    }

    fn finish(self) -> Result<(PathBuf, Format, u64), String> {
        let Self {
            path,
            format,
            writer,
            frames,
            ..
        } = self;
        match writer {
            ActiveWriter::Text { mut writer, .. } => {
                writer
                    .flush()
                    .map_err(|error| format!("刷新记录文件失败: {error}"))?;
                writer
                    .get_ref()
                    .sync_data()
                    .map_err(|error| format!("同步记录文件到磁盘失败: {error}"))?;
            }
            ActiveWriter::Blf(writer) => {
                writer.finish()?;
            }
        }
        Ok((path, format, frames))
    }
}

pub struct Recorder {
    control_tx: Sender<Control>,
    frame_tx: Sender<Vec<CanFrame>>,
    event_rx: Receiver<Event>,
    dropped_frames: Arc<AtomicU64>,
    high_watermark: Arc<AtomicUsize>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    pub fn spawn() -> Self {
        let (control_tx, control_rx) = bounded(CONTROL_QUEUE_CAPACITY);
        let (frame_tx, frame_rx) = bounded::<Vec<CanFrame>>(FRAME_QUEUE_CAPACITY);
        let (event_tx, event_rx) = bounded(EVENT_QUEUE_CAPACITY);
        let join = std::thread::Builder::new()
            .name("pcanwork-recorder".into())
            .spawn(move || worker(control_rx, frame_rx, event_tx))
            .expect("failed to start recorder thread");
        Self {
            control_tx,
            frame_tx,
            event_rx,
            dropped_frames: Arc::new(AtomicU64::new(0)),
            high_watermark: Arc::new(AtomicUsize::new(0)),
            join: Some(join),
        }
    }

    pub fn start(&self, path: PathBuf, format: Format) -> Result<(), String> {
        self.control_tx
            .try_send(Control::Start { path, format })
            .map_err(|_| "记录控制队列忙，无法开始记录".to_string())
    }

    pub fn stop(&self) -> Result<(), String> {
        self.control_tx
            .try_send(Control::Stop)
            .map_err(|_| "记录控制队列忙，无法停止记录".to_string())
    }

    pub fn push(&self, frame: CanFrame) -> Result<(), String> {
        self.push_batch(vec![frame])
    }

    pub fn push_batch(&self, frames: Vec<CanFrame>) -> Result<(), String> {
        if frames.is_empty() {
            return Ok(());
        }
        let frame_count = frames.len() as u64;
        match self.frame_tx.try_send(frames) {
            Ok(()) => {
                self.high_watermark
                    .fetch_max(self.frame_tx.len(), Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped_frames
                    .fetch_add(frame_count, Ordering::Relaxed);
                Err(format!(
                    "记录队列已满，拒绝静默丢帧；本次未写入 {frame_count} 帧"
                ))
            }
        }
    }

    pub fn try_event(&self) -> Option<Event> {
        self.event_rx.try_recv().ok()
    }

    pub fn queue_depth(&self) -> usize {
        self.frame_tx.len()
    }

    pub fn queue_capacity(&self) -> usize {
        FRAME_QUEUE_CAPACITY
    }

    pub fn queue_high_watermark(&self) -> usize {
        self.high_watermark.load(Ordering::Relaxed)
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    pub fn begin_shutdown(&mut self) -> Option<std::thread::JoinHandle<()>> {
        let _ = self.control_tx.send(Control::Shutdown);
        self.join.take()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.control_tx.try_send(Control::Shutdown);
    }
}

fn emit(event_tx: &Sender<Event>, event: Event) {
    let _ = event_tx.try_send(event);
}

fn finish_active(active: &mut Option<ActiveRecording>, event_tx: &Sender<Event>) {
    let Some(recording) = active.take() else {
        return;
    };
    match recording.finish() {
        Ok((path, format, frames)) => emit(
            event_tx,
            Event::Stopped {
                path,
                format,
                frames,
            },
        ),
        Err(error) => emit(event_tx, Event::Failed(error)),
    }
}

fn drain_pending_frames(
    active: &mut Option<ActiveRecording>,
    frame_rx: &Receiver<Vec<CanFrame>>,
) -> Result<(), String> {
    while let Ok(frames) = frame_rx.try_recv() {
        if let Some(recording) = active.as_mut() {
            recording.write_batch(&frames)?;
        }
    }
    Ok(())
}

fn worker(
    control_rx: Receiver<Control>,
    frame_rx: Receiver<Vec<CanFrame>>,
    event_tx: Sender<Event>,
) {
    let mut active: Option<ActiveRecording> = None;
    loop {
        select_biased! {
            recv(control_rx) -> command => match command {
                Ok(Control::Start { path, format }) => {
                    finish_active(&mut active, &event_tx);
                    match ActiveRecording::create(path.clone(), format) {
                        Ok(recording) => {
                            active = Some(recording);
                            emit(&event_tx, Event::Started { path, format });
                        }
                        Err(error) => emit(&event_tx, Event::Failed(error)),
                    }
                }
                Ok(Control::Stop) => {
                    if let Err(error) = drain_pending_frames(&mut active, &frame_rx) {
                        active = None;
                        emit(&event_tx, Event::Failed(error));
                    } else {
                        finish_active(&mut active, &event_tx);
                    }
                }
                Ok(Control::Shutdown) | Err(_) => {
                    if let Err(error) = drain_pending_frames(&mut active, &frame_rx) {
                        drop(active.take());
                        emit(&event_tx, Event::Failed(error));
                    } else {
                        finish_active(&mut active, &event_tx);
                    }
                    return;
                }
            },
            recv(frame_rx) -> frames => match frames {
                Ok(frames) => {
                    if let Some(recording) = active.as_mut()
                        && let Err(error) = recording.write_batch(&frames)
                    {
                        active = None;
                        emit(&event_tx, Event::Failed(error));
                    }
                }
                Err(_) => {
                    finish_active(&mut active, &event_tx);
                    return;
                }
            },
            default(CHECKPOINT_INTERVAL) => {
                if let Some(recording) = active.as_mut()
                    && let Err(error) = recording.checkpoint()
                {
                    active = None;
                    emit(&event_tx, Event::Failed(error));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(index: u32) -> CanFrame {
        CanFrame {
            t: index as f64 / 1000.0,
            ch: 1,
            tx: false,
            id: 0x100 + index,
            ext: false,
            fd: false,
            brs: false,
            remote: false,
            error: false,
            data: vec![index as u8],
        }
    }

    #[test]
    fn recorder_streams_and_finishes_blf() {
        let path = std::env::temp_dir().join("pcanwork_recorder_test.blf");
        let mut recorder = Recorder::spawn();
        recorder.start(path.clone(), Format::Blf).unwrap();
        for index in 0..32 {
            recorder.push(frame(index)).unwrap();
        }
        recorder.stop().unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stopped = false;
        while Instant::now() < deadline {
            if let Some(Event::Stopped { frames, .. }) = recorder.try_event() {
                assert_eq!(frames, 32);
                stopped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(stopped, "recorder did not stop within timeout");
        let back = blf::read(&path.to_string_lossy()).unwrap();
        assert_eq!(back.len(), 32);
        if let Some(join) = recorder.begin_shutdown() {
            join.join().unwrap();
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn asc_header_uses_current_year() {
        let path = std::env::temp_dir().join("pcanwork_recorder_test.asc");
        let recording = ActiveRecording::create(path.clone(), Format::Asc).unwrap();
        recording.finish().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("date "));
        assert!(text.contains(&Local::now().format("%Y").to_string()));
        assert!(!text.contains("Tue Jun 17 10:00:00 am 2026"));
        let _ = std::fs::remove_file(path);
    }
}
