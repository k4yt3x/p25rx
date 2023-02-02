#[macro_use]
extern crate serde_derive;

#[macro_use]
extern crate serde_json;

#[macro_use]
extern crate log;

extern crate arrayvec;
extern crate chan;
extern crate chrono;
extern crate clap;
extern crate collect_slice;
extern crate crossbeam;
extern crate demod_fm;
extern crate env_logger;
extern crate fnv;
extern crate imbe;
extern crate libc;
extern crate mio;
extern crate mio_extras;
extern crate moving_avg;
extern crate num;
extern crate p25;
extern crate p25_filts;
extern crate pool;
extern crate prctl;
extern crate rtlsdr_iq;
extern crate rtlsdr_mt;
extern crate serde;
extern crate slice_cast;
extern crate slice_mip;
extern crate static_decimate;
extern crate static_fir;
extern crate throttle;
extern crate uhttp_chunked_write;
extern crate uhttp_json_api;
extern crate uhttp_method;
extern crate uhttp_response_header;
extern crate uhttp_sse;
extern crate uhttp_status;
extern crate uhttp_uri;
extern crate uhttp_version;

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::Path,
    sync::mpsc::channel,
};

use anyhow::Result;
use clap::Parser;
use env_logger::{Builder, Env};
use log::LevelFilter;
use rtlsdr_mt::TunerGains;

mod audio;
mod consts;
mod demod;
mod http;
mod hub;
mod policy;
mod recv;
mod replay;
mod sdr;
mod talkgroups;

use audio::{AudioOutput, AudioTask};
use consts::{BASEBAND_SAMPLE_RATE, SDR_SAMPLE_RATE};
use demod::DemodTask;
use hub::HubTask;
use policy::ReceiverPolicy;
use recv::RecvTask;
use replay::ReplayReceiver;
use sdr::{ControlTask, ReadTask};
use talkgroups::TalkgroupSelection;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// enable verbose logging (pass twice to be extra verbose)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// ppm frequency adjustment
    #[arg(short, long, default_value_t = 0)]
    ppm: i32,

    /// file/fifo for audio samples (f32le/8kHz/mono)
    #[arg(short, long, required = true)]
    audio: String,

    /// tuner gain (use -g list to see all options)
    #[arg(short, long, required = true)]
    gain: String,

    /// replay from baseband samples in FILE
    #[arg(short, long)]
    replay: Option<String>,

    /// write baseband samples to FILE (f32le/48kHz/mono)
    #[arg(short, long)]
    write: Option<String>,

    /// frequency for initial control channel (Hz)
    #[arg(short, long, required = true)]
    freq: u32,

    /// rtlsdr device index (use -d list to show all)
    #[arg(short, long, default_value = "0")]
    device: String,

    /// HTTP socket bind address
    #[arg(short, long, default_value = "0.0.0.0:8025")]
    bind: String,

    /// disable frequency hopping
    #[arg(short, long)]
    nohop: bool,

    /// time (sec) to wait for voice message to be resumed
    #[arg(short, long = "pause-timeout", default_value_t = 2.0)]
    pause: f32,

    /// time (sec) to wait for voice message to begin
    #[arg(short, long = "watchdog-timeout", default_value_t = 2.0)]
    watchdog: f32,

    /// time (sec) to collect talkgrouops before making a selection
    #[arg(short, long = "tgselect-timeout", default_value_t = 1.0)]
    tgselect: f32,
}

fn main() -> Result<()> {
    let args = Args::parse();

    {
        let level = match args.verbose {
            0 => LevelFilter::Info,
            1 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        };

        Builder::from_env(Env::default()).filter(None, level).init();
    }

    let audio_out = || {
        let path = args.audio;
        info!("writing audio frames to {}", path);

        // Create audio file if it does not exist.
        match Path::new(&path).exists() {
            true => {
                info!("File {path} already exists, no need to create it.");
            }
            false => {
                match File::create(&path) {
                    Ok(_) => {
                        info!("File {path} created, ready to use.");
                    }
                    Err(e) => {
                        panic!("Unable to create file {path} due to error: {e}");
                    }
                }
            }
        };
        
        AudioOutput::new(BufWriter::new(
            OpenOptions::new()
                .write(true)
                .open(path)
                .expect("unable to open audio output file"),
        ))
    };

    if let Some(path) = args.replay {
        let mut stream = File::open(path).expect("unable to open replay file");
        let mut recv = ReplayReceiver::new(audio_out());

        recv.replay(&mut stream);

        return Ok(());
    }

    let samples_file = args
        .write
        .map(|path| File::create(path).expect("unable to open baseband file"));

    let dev: u32 = match &args.device[..] {
        "list" => {
            for (idx, name) in rtlsdr_mt::devices().enumerate() {
                println!("{}: {}", idx, name.to_str().unwrap());
            }

            return Ok(());
        }
        s => s.parse().expect("invalid device index"),
    };

    info!("opening RTL-SDR at index {}", dev);
    let (mut control, reader) = rtlsdr_mt::open(dev).expect("unable to open rtlsdr");

    match &args.gain[..] {
        "list" => {
            let mut gains = TunerGains::default();

            for g in control.tuner_gains(&mut gains) {
                println!("{}", g);
            }

            println!("auto");

            return Ok(());
        }
        "auto" => {
            info!("enabling hardware AGC");
            control.enable_agc().expect("unable to enable agc");
        }
        s => {
            let gain = s.parse().expect("invalid gain");
            info!("setting hardware gain to {:.1} dB", gain as f32 / 10.0);
            control.set_tuner_gain(gain).expect("unable to set gain");
        }
    }

    let pause = time_samples(args.pause);
    let watchdog = time_samples(args.watchdog);
    let tgselect = time_samples(args.tgselect);

    info!("setting frequency offset to {} PPM", args.ppm);
    control.set_ppm(args.ppm).expect("unable to set ppm");
    control
        .set_sample_rate(SDR_SAMPLE_RATE)
        .expect("unable to set sample rate");

    info!("using control channel frequency {} Hz", args.freq);

    let (tx_ctl, rx_ctl) = channel();
    let (tx_recv, rx_recv) = channel();
    let (tx_read, rx_read) = channel();
    let (tx_audio, rx_audio) = channel();
    let (tx_hub, rx_hub) = mio_extras::channel::channel();

    let policy = ReceiverPolicy::new(tgselect, watchdog, pause);
    let talkgroups = TalkgroupSelection::default();

    info!("starting HTTP server at http://{}", args.bind);
    let mut hub = HubTask::new(rx_hub, tx_recv.clone(), &args.bind.parse()?)?;
    let mut control = ControlTask::new(control, rx_ctl);
    let mut read = ReadTask::new(tx_read);
    let mut demod = DemodTask::new(rx_read, tx_hub.clone(), tx_recv.clone());
    let mut recv = RecvTask::new(
        rx_recv,
        tx_hub.clone(),
        tx_ctl.clone(),
        tx_audio.clone(),
        args.freq,
        !args.nohop,
        policy,
        talkgroups,
    );
    let mut audio = AudioTask::new(audio_out(), rx_audio);

    crossbeam::scope(|scope| {
        scope.spawn(move || {
            prctl::set_name("hub").unwrap();
            hub.run();
        });

        scope.spawn(move || {
            prctl::set_name("controller").unwrap();
            control.run()
        });

        scope.spawn(move || {
            prctl::set_name("reader").unwrap();
            read.run(reader);
        });

        scope.spawn(move || {
            prctl::set_name("demod").unwrap();
            demod.run();
        });

        scope.spawn(move || {
            prctl::set_name("receiver").unwrap();

            if let Some(mut f) = samples_file {
                recv.run(|samples| {
                    f.write_all(unsafe { slice_cast::cast(samples) })
                        .expect("unable to write baseband");
                })
            }
            else {
                recv.run(|_| {})
            }
        });

        scope.spawn(move || {
            prctl::set_name("audio").unwrap();
            audio.run();
        });
    });

    Ok(())
}

/// Convert the given seconds into an amount of baseband samples.
fn time_samples(t: f32) -> usize {
    (t * BASEBAND_SAMPLE_RATE as f32) as usize
}
