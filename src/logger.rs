use std::{fs, io::Write, sync::Mutex};

pub struct DualLogger {
    inner: env_logger::Logger,
    warn_file: Option<Mutex<fs::File>>, // 仅记录 warn 级别到文件
    crate_prefix: String,
}

impl log::Log for DualLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let target = metadata.target();
        if !target.starts_with(&self.crate_prefix) {
            return false;
        }
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            // 保持原有行为：输出到控制台（由 env_logger 处理）
            self.inner.log(record);

            // 额外写入：将 warn 级别日志导出到根目录 warnings.log
            if record.level() == log::Level::Warn
                && let Some(f) = &self.warn_file
                && let Ok(mut file) = f.lock()
            {
                let _ = writeln!(file, "[WARN] {} - {}", record.target(), record.args());
            }
        }
    }

    fn flush(&self) {
        self.inner.flush();
        if let Some(f) = &self.warn_file
            && let Ok(mut file) = f.lock()
        {
            let _ = file.flush();
        }
    }
}

pub fn init_logger() {
    let inner =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();

    // 打开根目录下的 warnings.log（失败不影响控制台日志）
    let warn_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("warnings.log")
        .ok()
        .map(Mutex::new);

    let crate_prefix = env!("CARGO_PKG_NAME").replace('-', "_");
    let dual = DualLogger {
        inner,
        warn_file,
        crate_prefix,
    };
    let _ = log::set_boxed_logger(Box::new(dual));
    // 放开最大级别，具体过滤由 inner.enabled 控制
    log::set_max_level(log::LevelFilter::Trace);
}
