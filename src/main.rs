use clap::Parser;
use log::{info, warn, error};
use nvml::NVML;
use std::fs::{OpenOptions, read_to_string};
use std::io::Write;
use std::path::Path;
use std::process::exit;
use std::{thread, time};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 命令行参数
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
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

/// log初始化
fn init_log() {
    env_logger::Builder::from_default_env()
        .format(|buf, record| {
            use std::io::Write;
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

/// 从 pwm 路径推测 enable 路径
fn find_enable_path(pwm_path: &str) -> String {
    format!("{}_enable", pwm_path)
}

/// 读取 enable 状态
fn read_enable_mode(enable_path: &str) -> Option<u8> {
    match read_to_string(enable_path) {
        Ok(s) => s.trim().parse::<u8>().ok(),
        Err(_) => None,
    }
}

/// 设置 PWM 控制模式 (1=手动, 2=自动)
fn set_pwm_mode(pwm_path: &str, mode: u8) -> bool {
    let enable_path = find_enable_path(pwm_path);
    if !Path::new(&enable_path).exists() {
        error!("PWM使能文件不存在: {}", enable_path);
        return false;
    }
    if let Some(current) = read_enable_mode(&enable_path) {
        if current != mode {
            if let Ok(mut f) = OpenOptions::new().write(true).open(&enable_path) {
                if let Err(e) = f.write_all(mode.to_string().as_bytes()) {
                    error!("无法设置PWM模式: {}", e);
                    return false;
                }
                info!("PWM模式已设置为: {}", if mode == 1 {"手动"} else {"自动"});
            } else {
                error!("无法打开PWM使能文件: {}", enable_path);
                return false;
            }
        }
        true
    } else {
        error!("读取PWM模式失败: {}", enable_path);
        false
    }
}

/// 设置风扇转速 (0-255)
fn set_fan_speed(pwm_path: &str, speed: u8) -> bool {
    let val = speed.to_string();
    match OpenOptions::new().write(true).open(pwm_path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(val.as_bytes()) {
                error!("无法写入风扇控制文件 {}: {}", pwm_path, e);
                return false;
            }
            true
        }
        Err(e) => {
            error!("无法打开风扇控制文件 {}: {}", pwm_path, e);
            false
        }
    }
}

/// 硬编码温度到PWM（避免任何CPU运算、无线性插值）
/// 25°C及以下77；26~59查表；60及以上255
fn calculate_fan_speed(temp: u32) -> u8 {
    // 25°C及以下
    if temp <= 25 {
        77
    } else if temp >= 60 {
        255
    } else {
        // 26~59°C
        // Python线性公式: ((temp-25)*(255-77)//(60-25))+77
        // 预先查表
        const LUT: [u8; 34] = [
            82, 87, 92, 97, 102, 107, 112, 117, 122, 127, 132, 137,
            142, 147, 152, 157, 162, 167, 172, 177, 182, 187, 192,
            197, 202, 207, 212, 217, 222, 227, 232, 237, 242, 247
        ];
        LUT[(temp-26) as usize]
    }
}

/// 获取GPU温度
fn get_gpu_temp(nvml: &NVML) -> Option<u32> {
    let device = nvml.device_by_index(0).ok()?;
    match device.temperature(nvml::device::TemperatureSensor::Gpu) {
        Ok(t) => Some(t as u32),
        Err(e) => {
            error!("无法读取GPU温度: {:?}", e);
            None
        }
    }
}

/// 打印所有GPU的详细信息
fn print_gpu_info(nvml: &NVML) {
    match nvml.device_count() {
        Ok(count) => info!("系统中发现 {} 个GPU设备", count),
        Err(e) => { error!("获取GPU数量失败: {:?}", e); return; }
    }
    let count = nvml.device_count().unwrap();
    for i in 0..count {
        let device = match nvml.device_by_index(i) {
            Ok(d) => d,
            Err(e) => { error!("获取GPU {}失败: {:?}", i, e); continue; }
        };
        let name = device.name().unwrap_or_else(|_| "Unknown".into());
        let temp = device.temperature(nvml::device::TemperatureSensor::Gpu)
            .unwrap_or(0);
        let mem = device.memory_info().ok();
        let power = device.power_usage().ok().map(|p| (p as f32)/1000.0);
        let fan = device.fan_speed().ok();

        info!("\nGPU {}: {}", i, name);
        info!("温度: {}°C", temp);
        if let Some(m) = mem {
            info!("显存: 已用 {}MB / 总共 {}MB (剩余 {}MB)",
                m.used/1024/1024, m.total/1024/1024, m.free/1024/1024);
        }
        if let Some(p) = power {
            info!("功耗: {:.1}W", p);
        }
        if let Some(f) = fan {
            info!("风扇转速: {}%", f);
        }
    }
}

fn main() {
    init_log();
    let args = Args::parse();

    let nvml = match NVML::init() {
        Ok(n) => n,
        Err(e) => {
            error!("初始化NVML失败: {:?}", e);
            exit(1);
        }
    };

    if args.info {
        print_gpu_info(&nvml);
        nvml.shutdown().ok();
        return;
    }

    let pwm_path = match args.pwm_path {
        Some(ref p) => p,
        None => {
            error!("需要提供PWM控制文件路径，除非使用--info选项");
            exit(1);
        }
    };

    if !Path::new(pwm_path).exists() {
        error!("PWM控制文件不存在: {}", pwm_path);
        exit(1);
    }

    let interval = if args.interval < 0.1 {
        warn!("检查间隔过短，已设为0.1s");
        0.1
    } else {
        args.interval
    };

    // 设置为手动模式
    if !set_pwm_mode(pwm_path, 1) {
        error!("无法设置PWM为手动模式，程序退出");
        exit(1);
    }

    // 退出时清理
    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        let pwm_path = pwm_path.clone();
        ctrlc::set_handler(move || {
            running.store(false, Ordering::SeqCst);
            set_fan_speed(&pwm_path, 77);
            set_pwm_mode(&pwm_path, 2);
        }).expect("无法设置 Ctrl-C 处理");
    }

    // 主循环
    info!("正在监控GPU温度，使用PWM路径: {}", pwm_path);
    let mut last_temp = 0;
    let mut last_speed = 0;
    while running.load(Ordering::SeqCst) {
        let temp = get_gpu_temp(&nvml).unwrap_or(0);
        let speed = if temp > 0 { calculate_fan_speed(temp) } else { 77 };
        // 仅当温度或转速变化时才写入
        if temp != last_temp || speed != last_speed {
            set_fan_speed(pwm_path, speed);
            if temp > 0 {
                info!("温度: {}°C, 风扇转速: {}/255 ({}%)", temp, speed, speed as u32 * 100 / 255);
            } else {
                warn!("无法获取GPU温度，使用默认转速");
            }
            last_temp = temp;
            last_speed = speed;
        }
        thread::sleep(time::Duration::from_secs_f64(interval));
    }

    // 程序退出清理
    set_fan_speed(pwm_path, 77);
    set_pwm_mode(pwm_path, 2);
    nvml.shutdown().ok();
    info!("程序已停止");
}