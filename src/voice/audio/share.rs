//! Event-driven Windows audio loopback for an explicitly enabled Go Live share.

use super::{CHANNELS, Captured, FRAME_SAMPLES, SAMPLE_RATE, SILENCE_FRAME, dsp};
use crate::voice::video::ScreenSource;
use opus_rs::{Application, OpusEncoder};
use std::mem::ManuallyDrop;
use std::sync::{Arc, mpsc as std_mpsc};
use std::time::Duration;
use tokio::sync::mpsc;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{
    BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, IAgileObject, IAgileObject_Impl,
    StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0},
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::core::{Error, HRESULT, IUnknown, Interface, PCWSTR, Ref, Result, implement};

/// An owned Win32 event. Its handle can be waited on or signalled from any thread.
struct Event(HANDLE);

unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    fn new() -> Result<Self> {
        unsafe { CreateEventW(None, false, false, PCWSTR::null()).map(Self) }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Dropping this handle wakes the capture worker immediately and ends the share's audio.
pub(crate) struct ShareAudio {
    stop: Arc<Event>,
}

impl ShareAudio {
    pub(crate) fn start(
        source: ScreenSource,
        captured: mpsc::Sender<Captured>,
        report: impl FnOnce(String) + Send + 'static,
    ) -> std::result::Result<Self, String> {
        let me = std::process::id();
        let (pid, mode) = match source {
            ScreenSource::Window(window) => {
                let mut pid = 0;
                unsafe {
                    GetWindowThreadProcessId(HWND(window as usize as *mut _), Some(&mut pid))
                };
                if pid == 0 || pid == me {
                    return Err("Audio is unavailable for this window.".into());
                }
                (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE)
            }
            ScreenSource::Monitor(_) | ScreenSource::DesktopDuplication(_) => {
                (me, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE)
            }
        };
        let stop = Arc::new(Event::new().map_err(|e| e.to_string())?);
        let worker_stop = stop.clone();
        std::thread::Builder::new()
            .name("rustcord-share-audio".into())
            .spawn(move || {
                if let Err(error) = run(pid, mode, &worker_stop, &captured) {
                    report(format!(
                        "Could not share audio. The video can continue. ({error})"
                    ));
                }
                let _ = captured.try_send(Captured::End);
            })
            .map_err(|e| format!("Could not start screen share audio: {e}"))?;
        Ok(Self { stop })
    }
}

impl Drop for ShareAudio {
    fn drop(&mut self) {
        let _ = unsafe { SetEvent(self.stop.0) };
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Completion(std_mpsc::Sender<()>);

impl IAgileObject_Impl for Completion_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(&self, _: Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

fn activate(pid: u32, mode: PROCESS_LOOPBACK_MODE) -> Result<IAudioClient> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: mode,
            },
        },
    };
    // A regular PROPVARIANT drops with PropVariantClear, which would free the borrowed BLOB.
    let variant = ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&mut params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast(),
                    },
                },
                ..Default::default()
            }),
        },
    });
    let (tx, rx) = std_mpsc::channel();
    let completion: IActivateAudioInterfaceCompletionHandler = Completion(tx).into();
    let activation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*variant),
            &completion,
        )?
    };
    rx.recv_timeout(Duration::from_secs(3))
        .map_err(|e| Error::new(E_FAIL, format!("Audio activation did not complete: {e}")))?;
    let mut status = HRESULT::default();
    let mut interface: Option<IUnknown> = None;
    unsafe { activation.GetActivateResult(&mut status, &mut interface)? };
    status.ok()?;
    interface.ok_or_else(|| Error::from(E_FAIL))?.cast()
}

struct Com;

impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn run(
    pid: u32,
    mode: PROCESS_LOOPBACK_MODE,
    stop: &Event,
    captured: &mpsc::Sender<Captured>,
) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let _com = Com;
    let ready = Event::new()?;
    let client = activate(pid, mode)?;
    const BLOCK_ALIGN: u16 = (CHANNELS * size_of::<i16>()) as u16;
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * u32::from(BLOCK_ALIGN),
        nBlockAlign: BLOCK_ALIGN,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            0,
            0,
            &format,
            None,
        )?;
        client.SetEventHandle(ready.0)?;
    }
    let capture: IAudioCaptureClient = unsafe { client.GetService()? };
    // The virtual loopback device can report invalid endpoint capacity. Size this scratch
    // buffer for the usual packet and use the actual packet size when draining.
    let mut pcm = Vec::with_capacity(FRAME_SAMPLES * CHANNELS);
    let mut pack = StereoFrames::new().map_err(|e| Error::new(E_FAIL, e))?;
    unsafe { client.Start()? };
    let handles = [stop.0, ready.0];
    loop {
        match unsafe { WaitForMultipleObjects(&handles, false, INFINITE) } {
            WAIT_OBJECT_0 => return unsafe { client.Stop() },
            event if event.0 == WAIT_OBJECT_0.0 + 1 => {}
            _ => return Err(Error::from_thread()),
        }
        while unsafe { capture.GetNextPacketSize()? } != 0 {
            let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0, 0);
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };
            if frames > SAMPLE_RATE {
                unsafe { capture.ReleaseBuffer(frames)? };
                return Err(Error::new(E_FAIL, "Unexpectedly large screen audio packet"));
            }
            let samples = frames as usize * CHANNELS;
            pcm.clear();
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                pcm.resize(samples, 0);
            } else {
                // WASAPI owns this aligned PCM buffer until ReleaseBuffer.
                pcm.extend_from_slice(unsafe { std::slice::from_raw_parts(data.cast(), samples) });
            }
            unsafe { capture.ReleaseBuffer(frames)? };
            pack.push(&pcm, captured);
        }
    }
}

const SAMPLES: usize = FRAME_SAMPLES * CHANNELS;

struct StereoFrames {
    frame: [f32; SAMPLES],
    used: usize,
    encoder: OpusEncoder,
    trailer: dsp::Trailer,
}

impl StereoFrames {
    fn new() -> std::result::Result<Self, &'static str> {
        let mut encoder = OpusEncoder::new(SAMPLE_RATE as i32, CHANNELS, Application::Audio)?;
        encoder.bitrate_bps = 96_000;
        Ok(Self {
            frame: [0.0; SAMPLES],
            used: 0,
            encoder,
            trailer: dsp::Trailer::default(),
        })
    }

    fn push(&mut self, mut samples: &[i16], captured: &mpsc::Sender<Captured>) {
        while !samples.is_empty() {
            let take = samples.len().min(SAMPLES - self.used);
            for (out, &sample) in self.frame[self.used..self.used + take]
                .iter_mut()
                .zip(samples)
            {
                *out = f32::from(sample) / 32768.0;
            }
            self.used += take;
            samples = &samples[take..];
            if self.used != SAMPLES {
                continue;
            }
            self.used = 0;
            let nonzero = self.frame.iter().any(|&s| s != 0.0);
            let packet = match self.trailer.next(nonzero) {
                dsp::Tx::Voice => {
                    dsp::encode_stereo(&mut self.encoder, &self.frame).map(Captured::Frame)
                }
                dsp::Tx::Silence => Some(Captured::Frame(SILENCE_FRAME.to_vec())),
                dsp::Tx::End => Some(Captured::End),
                dsp::Tx::Nothing => None,
            };
            if let Some(packet) = packet {
                let _ = captured.try_send(packet);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_stereo_frames_are_encoded_once() {
        let mut pack = StereoFrames::new().unwrap();
        let (send, mut recv) = mpsc::channel(4);
        let pcm: [i16; SAMPLES] = std::array::from_fn(|i| {
            if i % 2 == 0 {
                (8000.0
                    * ((i / 2) as f32 * std::f32::consts::TAU * 440.0 / SAMPLE_RATE as f32).sin())
                    as i16
            } else {
                0
            }
        });
        pack.push(&pcm[..256], &send);
        assert!(recv.try_recv().is_err());
        pack.push(&pcm[256..], &send);
        let Captured::Frame(frame) = recv.try_recv().unwrap() else {
            panic!("expected Opus")
        };
        let mut decoded = [0.0; SAMPLES];
        opus_rs::OpusDecoder::new(SAMPLE_RATE as i32, CHANNELS)
            .unwrap()
            .decode(&frame, FRAME_SAMPLES, &mut decoded)
            .unwrap();
        assert!(decoded.iter().any(|sample| sample.abs() > 0.10));
        assert!(recv.try_recv().is_err());
    }

    #[test]
    #[ignore = "requires an interactive Windows audio service; does not play or send audio"]
    fn opens_process_loopback_on_this_windows_installation() {
        for mode in [
            PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
        ] {
            let stop = Event::new().unwrap();
            unsafe { SetEvent(stop.0).unwrap() };
            let (tx, mut rx) = mpsc::channel(1);
            run(std::process::id(), mode, &stop, &tx).unwrap();
            assert!(rx.try_recv().is_err());
        }
    }
}
