//! Receiver orchestration.
//!
//! M0 wires an [`IqSource`] to the signal analyser that feeds the GUI's
//! spectrum and constellation views. The demodulator, FEC and GSE stages slot
//! in here from M1 onward — see `docs/DESIGN.md`.

pub mod beacon;
pub mod bursty;
pub mod carriers;
pub mod cid;
pub mod demod;
pub mod estimate;
pub mod fec;
pub mod floor;
pub mod frontend;
pub mod identify;
pub mod period;
pub mod plan_feed;
pub mod programme;
pub mod psk;
pub mod radar;
pub mod rf;
pub mod spectrum;
pub mod streams;
pub mod superframe;
pub mod tdma;
pub mod tx;
pub mod vfo;
pub mod wireshark;

pub use carriers::{Carrier, detect_carriers};
pub use cid::CidView;
pub use decsat_modem::dvbs::qef_es_n0_db as dvbs_qef_es_n0_db;
pub use decsat_modem::e1::Coding as E1Coding;
/// The Intelsat 19 DAMA outbound's receiver and what it shows.
pub use decsat_modem::is19;
pub use decsat_modem::tdm257::{
    ChannelState as TdmChannelState, TdmStats, VoiceView as TdmVoiceView,
};
pub use decsat_modem::tdmfast::{FastCall, FastStats as FastTdmStats};
pub use decsat_modem::text::{ByteTextView, TextView};
pub use decsat_modem::tpc2964::Code as TpcCode;
pub use demod::{Demod, LockState, PlFrame};
pub use estimate::{BandEstimate, estimate_band};
pub use fec::{
    BbFrame, CallVoiceView, E1View, FecDecoder, FecOutput, FecStats, GseView, MIX as VOICE_MIX,
    TerminalView, TsView, hdlc_ip,
};
pub use frontend::{Engine, EngineOptions, FrontStatus, STRONG_INPUT_DBFS, SourceState};
pub use identify::{
    ConstellationGuess, Identification, RateSource, Verdict, identify, identify_in, names_beacon,
    names_is19, names_is19_call, names_tpc,
};
pub use programme::ProgrammeView;
pub use psk::PskDemod;
pub use radar::RadarStageView;
pub use spectrum::Spectrum;
pub use streams::{StreamContent, StreamView};
pub use tdma::{BurstMark, ChannelView as TdmaChannelView, TdmaView};
pub use vfo::{CarrierState, DecoderKind, SignalMode, VfoId, VfoSettings, VfoStatus};

use decsat_core::{Iq, Metrics, Result, RxConfig};
use decsat_io::IqSource;

/// How many IQ samples a snapshot keeps for the scatter plot. More than a few
/// thousand points is wasted on screen and costs frame time.
const SCATTER_POINTS: usize = 4096;

/// One analysis/decode step's worth of results for the UI.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Decimated baseband samples for the constellation/scatter view.
    pub scatter: Vec<Iq>,
    /// Power spectrum in dB, lowest frequency first (FFT-shifted).
    pub spectrum_db: Vec<f32>,
    /// Receiver metrics as of this step.
    pub metrics: Metrics,
    /// Samples consumed in this step.
    pub samples: usize,
}

/// The receive chain.
pub struct Receiver {
    source: Box<dyn IqSource>,
    config: RxConfig,
    spectrum: Spectrum,
    metrics: Metrics,
    /// Reused input buffer — see the note on [`IqSource::read`].
    buf: Vec<Iq>,
}

impl Receiver {
    /// Build a receiver over `source`. `fft_size` sets the spectrum resolution.
    pub fn new(source: Box<dyn IqSource>, config: RxConfig, fft_size: usize) -> Self {
        Receiver {
            source,
            config,
            spectrum: Spectrum::new(fft_size),
            metrics: Metrics::default(),
            buf: Vec::new(),
        }
    }

    pub fn config(&self) -> &RxConfig {
        &self.config
    }

    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    pub fn describe_source(&self) -> String {
        self.source.describe()
    }

    /// Pull one block from the source and analyse it. Returns `None` at end of
    /// stream.
    pub fn step(&mut self) -> Result<Option<Snapshot>> {
        let n = self.source.read(&mut self.buf)?;
        if n == 0 {
            return Ok(None);
        }

        let spectrum_db = self.spectrum.compute(&self.buf);

        // Even decimation across the block, so the scatter view shows the whole
        // block rather than just its start.
        let stride = (self.buf.len() / SCATTER_POINTS).max(1);
        let scatter: Vec<Iq> = self.buf.iter().step_by(stride).copied().collect();

        self.metrics.frames_total = self.metrics.frames_total.saturating_add(1);

        Ok(Some(Snapshot {
            scatter,
            spectrum_db,
            metrics: self.metrics,
            samples: n,
        }))
    }
}
