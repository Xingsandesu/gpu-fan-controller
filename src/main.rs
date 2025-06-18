use clap::Parser;
use log::{info, warn, error};
use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use std::fs::{OpenOptions, read_to_string};
use std::io::Write;
use std::path::Path;
use std::process::exit;
use std::{thread, time};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use chrono;
use ctrlc;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// 风扇 PWM 控制文件路径
    pwm_path: Option<String>,

    /// 检查间隔(秒)
    #[arg(long, default_value_t = 2.0)]
    interval: f64,

    /// 显示GPU详细信息后退出
    #[arg(long)]
    info: bool,
}

fn init_log() {
    env_logger::Builder::from_default_env()
        .format(|buf, record| {
            writeln!(
                buf,
                "[{}] {:<8} {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                record.args()
            )
        })
        .init();
}

fn find_enable_path(pwm_path: &str) -> String {
    format!("{}_enable", pwm_path)
}

fn read_enable_mode(enable_path: &str) -> Option<u8> {
    read_to_string(enable_path).ok()?.trim().parse().ok()
}

fn set_pwm_mode(pwm_path: &str, mode: u8) -> bool {
    let enable = find_enable_path(pwm_path);
    if !Path::new(&enable).exists() {
        error!("PWM enable file not found: {}", enable);
        return false;
    }
    if let Some(current) = read_enable_mode(&enable) {
        if current != mode {
            if let Ok(mut f) = OpenOptions::new().write(true).open(&enable) {
                if f.write_all(mode.to_string().as_bytes()).is_err() {
                    error!("Failed to write PWM mode");
                    return false;
                }
                info!("PWM mode set to {}", if mode==1 { "manual" } else { "auto" });
            }
        }
        true
    } else {
        error!("Failed to read PWM mode from {}", enable);
        false
    }
}

fn set_fan_speed(pwm_path: &str, speed: u8) -> bool {
    if let Ok(mut f) = OpenOptions::new().write(true).open(pwm_path) {
        if f.write_all(speed.to_string().as_bytes()).is_ok() {
            true
        } else {
            error!("Failed to write fan speed to {}", pwm_path);
            false
        }
    } else {
        error!("Cannot open fan path {}", pwm_path);
        false
    }
}

fn calculate_fan_speed(temp: u32) -> u8 {
    if temp <= 25 {
        77
    } else if temp >= 60 {
        255
    } else {
        const LUT: [u8; 34] = [
            82,87,92,97,102,107,112,117,122,127,132,137,
            142,147,152,157,162,167,172,177,182,187,192,
            197,202,207,212,217,222,227,232,237,242,247
        ];
        LUT[(temp - 26) as usize]
    }
}

fn get_gpu_temp(nvml: &Nvml) -> Option<u32> {
    let d = nvml.device_by_index(0).ok()?;
    d.temperature(TemperatureSensor::Gpu).ok().map(|t| t as u32)
}

fn print_gpu_info(nvml: &Nvml) {
    match nvml.device_count() {
        Ok(c) => info!("Found {} GPU(s)", c),
        Err(e) => { error!("Failed to get device count: {:?}", e); return; }
    }
    for i in 0..nvml.device_count().unwrap() {
        let d = match nvml.device_by_index(i) {
            Ok(x) => x,
            Err(e) => { error!("Cannot get GPU {}: {:?}", i, e); continue; }
        };
        let name = d.name().unwrap_or_else(|_| "Unknown".into());
        let temp = d.temperature(TemperatureSensor::Gpu).unwrap_or(0);
        let mem = d.memory_info().ok();
        let pw = d.power_usage().ok().map(|p| p as f32 / 1000.0);
        let fan = d.fan_speed(0).ok();

        info!("\nGPU {}: {}", i, name);
        info!("Temp: {}°C", temp);
        if let Some(m) = mem {
            info!("Mem: Used {}MB / Total {}MB",
                m.used / 1024 / 1024, m.total / 1024 / 1024);
        }
        if let Some(p) = pw { info!("Power: {:.1} W", p); }
        if let Some(f) = fan { info!("Fan speed: {}%", f); }
    }
}

fn main() {
    init_log();
    let args = Args::parse();

    let nvml = Nvml::init().unwrap_or_else(|e| {
        error!("Failed to init NVML: {:?}", e);
        exit(1)
    });

    if args.info {
        print_gpu_info(&nvml);
        return;
    }

    let pwm = args.pwm_path.as_ref().unwrap_or_else(|| {
        error!("Must provide PWM path unless --info");
        exit(1);
    });

    if !Path::new(pwm).exists() {
        error!("PWM path does not exist: {}", pwm);
        exit(1);
    }

    let interval = if args.interval < 0.1 { warn!("Interval too low, use 0.1s"); 0.1 } else { args.interval };

    if !set_pwm_mode(pwm, 1) {
        error!("Failed to enable manual PWM mode");
        exit(1);
    }

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        let p = pwm.clone();
        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
            set_fan_speed(&p, 77);
            set_pwm_mode(&p, 2);
        }).expect("Cannot set Ctrl-C handler");
    }

    let (mut lt, mut ls) = (0, 0);
    info!("Monitoring GPU temp, PWM path: {}", pwm);
    while running.load(Ordering::SeqCst) {
        let t = get_gpu_temp(&nvml).unwrap_or(0);
        let sp = if t>0 { calculate_fan_speed(t) } else { 77 };
        if t!=lt || sp!=ls {
            set_fan_speed(pwm, sp);
            if t>0 {
                info!("Temp {}°C, speed {}/255 ({}%)", t, sp, sp as u32 * 100/255);
            } else {
                warn!("GPU temp unavailable, use default speed");
            }
            (lt, ls) = (t, sp);
        }
        thread::sleep(time::Duration::from_secs_f64(interval));
    }

    set_fan_speed(pwm, 77);
    set_pwm_mode(pwm, 2);
    info!("Exited cleanly");
}
