use clap::Parser;
use nvml_wrapper::{Nvml, enum_wrappers::device::TemperatureSensor};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write, Seek, SeekFrom},
    path::Path,
    process::exit,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::sleep,
    time::Duration,
};

#[derive(Parser, Debug)]
struct Args {
    pwm_path: Option<String>,
    #[arg(long, default_value_t = 2.0)]
    interval: f64,
    #[arg(long)]
    info: bool,
}

struct FileBuffer {
    path_buf: String,
    content_buf: String,
}

impl FileBuffer {
    fn new() -> Self {
        Self {
            path_buf: String::with_capacity(64),
            content_buf: String::with_capacity(16),
        }
    }

    fn make_enable_path(&mut self, pwm_path: &str) {
        self.path_buf.clear();
        self.path_buf.push_str(pwm_path);
        self.path_buf.push_str("_enable");
    }
}

struct CachedFiles {
    pwm_file: Option<File>,
    enable_file: Option<File>,
}

impl CachedFiles {
    fn new() -> Self {
        Self {
            pwm_file: None,
            enable_file: None,
        }
    }

    fn get_or_open_pwm(&mut self, path: &str) -> Option<&mut File> {
        if self.pwm_file.is_none() {
            self.pwm_file = OpenOptions::new().write(true).open(path).ok();
        }
        self.pwm_file.as_mut()
    }

    fn get_or_open_enable(&mut self, path: &str) -> Option<&mut File> {
        if self.enable_file.is_none() {
            self.enable_file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .ok();
        }
        self.enable_file.as_mut()
    }
}

struct FanController {
    nvml: Nvml,
    pwm_path: String,
    enable_path: String,
    last_temp: u32,
    last_speed: u8,
    buffer: FileBuffer,
    files: CachedFiles,
}

impl FanController {
    fn new(nvml: Nvml, pwm_path: String) -> Option<Self> {
        let mut buffer = FileBuffer::new();
        buffer.make_enable_path(&pwm_path);
        
        if !Path::new(&buffer.path_buf).exists() {
            return None;
        }

        let enable_path = buffer.path_buf.clone();
        let mut controller = Self {
            nvml,
            pwm_path,
            enable_path,
            last_temp: 0,
            last_speed: 0,
            buffer,
            files: CachedFiles::new(),
        };

        if !controller.set_pwm_mode(1) {
            return None;
        }

        Some(controller)
    }

    #[inline(always)]
    fn calculate_fan_speed(temp: u32) -> u8 {
        match temp {
            0..=25 => 77,
            26..=59 => 77 + ((temp - 25) * 5).min(178) as u8,
            _ => 255,
        }
    }

    #[inline(always)]
    fn get_gpu_temp(&self) -> Option<u32> {
        self.nvml
            .device_by_index(0)
            .ok()?
            .temperature(TemperatureSensor::Gpu)
            .ok()
            .map(|t| t as u32)
    }

    fn read_u8_from_enable_file(&mut self) -> Option<u8> {
        let enable_path = self.enable_path.clone();
        let file = self.files.get_or_open_enable(&enable_path)?;
        self.buffer.content_buf.clear();
        file.seek(SeekFrom::Start(0)).ok()?;
        file.read_to_string(&mut self.buffer.content_buf).ok()?;
        self.buffer.content_buf.trim().parse().ok()
    }

    fn write_u8_to_pwm_file(&mut self, val: u8) -> bool {
        let pwm_path = self.pwm_path.clone();
        if let Some(file) = self.files.get_or_open_pwm(&pwm_path) {
            file.seek(SeekFrom::Start(0)).is_ok()
                && file.write_all(val.to_string().as_bytes()).is_ok()
                && file.flush().is_ok()
        } else {
            false
        }
    }

    fn write_u8_to_enable_file(&mut self, val: u8) -> bool {
        let enable_path = self.enable_path.clone();
        if let Some(file) = self.files.get_or_open_enable(&enable_path) {
            file.seek(SeekFrom::Start(0)).is_ok()
                && file.write_all(val.to_string().as_bytes()).is_ok()
                && file.flush().is_ok()
        } else {
            false
        }
    }

    fn set_pwm_mode(&mut self, mode: u8) -> bool {
        if let Some(current) = self.read_u8_from_enable_file() {
            if current != mode {
                return self.write_u8_to_enable_file(mode);
            }
            return true;
        }
        false
    }

    fn set_fan_speed(&mut self, speed: u8) -> bool {
        self.write_u8_to_pwm_file(speed)
    }

    fn update(&mut self) {
        if let Some(temp) = self.get_gpu_temp() {
            let speed = Self::calculate_fan_speed(temp);
            if temp != self.last_temp || speed != self.last_speed {
                if self.set_fan_speed(speed) {
                    println!("温度: {}°C，风扇速度: {} / 255", temp, speed);
                    self.last_temp = temp;
                    self.last_speed = speed;
                }
            }
        } else if self.last_speed != 77 {
            if self.set_fan_speed(77) {
                println!("无法读取温度，使用默认速度 77");
                self.last_speed = 77;
            }
        }
    }

    fn cleanup(&mut self) {
        println!("正在执行清理...");
        let _ = self.set_fan_speed(77);
        let _ = self.set_pwm_mode(2);
    }
}

// **关键修复**：为 FanController 实现 Drop 特性
impl Drop for FanController {
    fn drop(&mut self) {
        self.cleanup();
    }
}

static RUNNING: AtomicBool = AtomicBool::new(true);

fn setup_signal_handler() {
    ctrlc::set_handler(|| {
        println!("\n接收到退出信号，正在准备关闭...");
        RUNNING.store(false, Ordering::Relaxed);
    })
    .unwrap_or_else(|_| {
        eprintln!("无法设置信号处理器");
    });
}

fn main() {
    let args = Args::parse();

    let nvml = Nvml::init().unwrap_or_else(|e| {
        eprintln!("无法初始化 NVML: {}", e);
        exit(1);
    });

    if args.info {
        if let Ok(count) = nvml.device_count() {
            for i in 0..count {
                if let Ok(device) = nvml.device_by_index(i) {
                    match device.temperature(TemperatureSensor::Gpu) {
                        Ok(temp) => println!("GPU {} 温度: {}°C", i, temp),
                        Err(e) => println!("GPU {} 温度读取失败: {}", i, e),
                    }
                }
            }
        }
        return;
    }

    let pwm_path = match args.pwm_path {
        Some(ref p) if Path::new(p).exists() => p.clone(),
        Some(ref p) => {
            eprintln!("PWM 路径不存在: {}", p);
            exit(1);
        }
        None => {
            eprintln!("必须指定 PWM 路径");
            exit(1);
        }
    };

    let controller = FanController::new(nvml, pwm_path).unwrap_or_else(|| {
        eprintln!("无法初始化风扇控制器");
        exit(1);
    });

    let controller_arc = Arc::new(Mutex::new(controller));
    setup_signal_handler();

    let sleep_nanos = (args.interval * 1_000_000_000.0) as u64;
    let sleep_duration = Duration::from_nanos(sleep_nanos);

    println!("风扇控制器已启动，监控间隔: {:.2}秒。按 Ctrl+C 退出。", args.interval);

    while RUNNING.load(Ordering::Relaxed) {
        {
            if let Ok(mut ctrl) = controller_arc.lock() {
                ctrl.update();
            }
        }
        sleep(sleep_duration);
    }

    println!("程序即将退出。");
    // **关键修复**：不再需要手动调用 cleanup。
    // 当 main 函数结束时，controller_arc 会被销毁，
    // 其内部的 FanController 的 drop 方法会自动被调用。
}