use eframe::egui;
use std::path::PathBuf;
use walkdir::WalkDir;
use bollard::Docker;
use bollard::container::{RestartContainerOptions, StopContainerOptions, ListContainersOptions, StartContainerOptions, LogsOptions, Stats, StatsOptions};
use bollard::exec::{CreateExecOptions};
use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use futures_util::stream::StreamExt;
use chrono::{DateTime, Local};
use std::process::Command;

#[allow(dead_code)]
#[derive(Deserialize)]
struct ComposeService {
    image: String,
    container_name: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct ComposeConfig {
    services: HashMap<String, ComposeService>,
}

#[derive(Default, Clone)]
struct ContainerStats {
    cpu_usage: f64,
    memory_usage: u64,
    memory_limit: u64,
    #[allow(dead_code)]
    network_rx: u64,
    #[allow(dead_code)]
    network_tx: u64,
    #[allow(dead_code)]
    block_read: u64,
    #[allow(dead_code)]
    block_write: u64,
    #[allow(dead_code)]
    last_update: Option<Instant>,
}

#[derive(Default, Clone)]
struct PortMapping {
    container_port: u16,
    host_port: u16,
    protocol: String,
}

#[derive(Clone)]
struct ContainerInfo {
    image: String,
    created: SystemTime,
    env: Vec<String>,
    volumes: Vec<(String, String)>, // (host_path, container_path)
}

impl Default for ContainerInfo {
    fn default() -> Self {
        Self {
            image: String::default(),
            created: UNIX_EPOCH,
            env: Vec::default(),
            volumes: Vec::default(),
        }
    }
}

#[derive(Default, Clone)]
struct ContainerState {
    name: String,
    status: String,
    logs: String,
    show_logs: bool,
    last_log_update: Option<Instant>,
    stats: ContainerStats,
    ports: Vec<PortMapping>,
    show_ports: bool,
    info: ContainerInfo,
    show_info: bool,
    expanded: bool,
}

#[derive(Default, Clone)]
struct DockerGuiApp {
    current_path: PathBuf,
    docker: Option<Docker>,
    containers: Arc<Mutex<HashMap<String, ContainerState>>>,
    compose_file: Option<PathBuf>,
    last_update: Option<Instant>,
}

impl DockerGuiApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let docker = Docker::connect_with_local_defaults().ok();
        Self {
            current_path: std::env::current_dir().unwrap_or_default(),
            docker,
            containers: Arc::new(Mutex::new(HashMap::new())),
            compose_file: None,
            last_update: None,
        }
    }

    fn check_compose_file(&mut self) {
        let compose_path = self.current_path.join("docker-compose.yml");
        if compose_path.exists() {
            self.compose_file = Some(compose_path.clone());
            
            // Читаем и парсим docker-compose.yml
            if let Ok(contents) = std::fs::read_to_string(&compose_path) {
                if let Ok(config) = serde_yaml::from_str::<ComposeConfig>(&contents) {
                    // Сохраняем имена контейнеров из compose файла
                    if let Ok(mut containers) = self.containers.lock() {
                        containers.clear();
                        // Предварительно заполняем HashMap пустыми состояниями для сохранения порядка
                        for (_, service) in config.services.iter() {
                            if let Some(name) = &service.container_name {
                                containers.insert(name.clone(), ContainerState::default());
                            }
                        }
                    }
                    self.update_containers_if_needed();
                }
            }
        } else {
            self.compose_file = None;
            if let Ok(mut containers) = self.containers.lock() {
                containers.clear();
            }
        }
    }

    fn update_containers_if_needed(&mut self) {
        let should_update = match self.last_update {
            None => true,
            Some(last) => last.elapsed() >= Duration::from_millis(500), // Обновляем каждые 500мс
        };

        if should_update {
            self.last_update = Some(Instant::now());
            let app_copy = self.clone();
            tokio::spawn(async move {
                if let Err(e) = app_copy.update_containers().await {
                    eprintln!("Error updating containers: {}", e);
                }
            });
        }
    }

    async fn update_containers(&self) -> Result<()> {
        if let Some(docker) = &self.docker {
            if let Some(compose_path) = &self.compose_file {
                // Читаем и парсим docker-compose.yml
                if let Ok(contents) = std::fs::read_to_string(compose_path) {
                    if let Ok(config) = serde_yaml::from_str::<ComposeConfig>(&contents) {
                        // Собираем множество имен контейнеров из compose файла
                        let compose_containers: std::collections::HashSet<String> = config.services
                            .iter()
                            .filter_map(|(_, service)| service.container_name.clone())
                            .collect();

                        let containers = docker.list_containers(Some(ListContainersOptions::<String> {
                            all: true,
                            ..Default::default()
                        })).await?;
                        
                        if let Ok(mut app_containers) = self.containers.lock() {
                            // Обновляем только те контейнеры, которые есть в compose файле
                            for container in containers {
                                if let (Some(id), Some(status), Some(names)) = (container.id, container.status, container.names) {
                                    let name = names.first()
                                        .map(|n| n.trim_start_matches('/').to_string())
                                        .unwrap_or_else(|| id.clone());
                                    
                                    // Проверяем, есть ли контейнер в compose файле
                                    if compose_containers.contains(&name) {
                                        let existing_state = app_containers.get(&name).cloned();
                                        
                                        let (show_logs, logs, last_log_update) = existing_state
                                            .map(|state| (state.show_logs, state.logs, state.last_log_update))
                                            .unwrap_or_default();

                                        app_containers.insert(name.clone(), ContainerState {
                                            name: name.clone(),
                                            status,
                                            logs,
                                            show_logs,
                                            last_log_update,
                                            stats: ContainerStats::default(),
                                            ports: Vec::new(),
                                            show_ports: false,
                                            info: ContainerInfo::default(),
                                            show_info: false,
                                            expanded: false,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn restart_container(&self, container_id: &str) -> Result<()> {
        if let Some(docker) = &self.docker {
            docker.restart_container(
                container_id,
                Some(RestartContainerOptions {
                    t: 10,
                })
            ).await?;
        }
        Ok(())
    }

    async fn stop_container(&self, container_id: &str) -> Result<()> {
        if let Some(docker) = &self.docker {
            docker.stop_container(
                container_id,
                Some(StopContainerOptions {
                    t: 10,
                })
            ).await?;
        }
        Ok(())
    }

    async fn start_container(&self, container_id: &str) -> Result<()> {
        if let Some(docker) = &self.docker {
            docker.start_container(container_id, None::<StartContainerOptions<String>>).await?;
        }
        Ok(())
    }

    async fn update_container_logs(&self, container_id: &str) -> Result<()> {
        if let Some(docker) = &self.docker {
            let should_update = {
                self.containers.lock()
                    .ok()
                    .and_then(|containers| containers.get(container_id).cloned())
                    .and_then(|state| state.last_log_update)
                    .map(|last| last.elapsed() >= Duration::from_secs(1))
                    .unwrap_or(true)
            };

            if should_update {
                let options = LogsOptions::<String> {
                    stdout: true,
                    stderr: true,
                    timestamps: true,
                    tail: "50".to_string(),
                    ..Default::default()
                };

                let mut logs_stream = docker.logs(container_id, Some(options));
                let mut new_logs = String::new();

                while let Some(log_result) = logs_stream.next().await {
                    if let Ok(log) = log_result {
                        if let Ok(log_str) = String::from_utf8(log.into_bytes().to_vec()) {
                            // Парсим временную метку и сообщение
                            if let Some((timestamp, message)) = log_str.split_once(' ') {
                                if let Ok(dt) = DateTime::parse_from_rfc3339(timestamp) {
                                    let local_time = dt.with_timezone(&chrono::Local);
                                    new_logs.push_str(&format!("[{}] {}\n", 
                                        local_time.format("%H:%M:%S"),
                                        message.trim()
                                    ));
                                }
                            }
                        }
                    }
                }

                if let Ok(mut containers) = self.containers.lock() {
                    if let Some(state) = containers.get_mut(container_id) {
                        state.logs = new_logs;
                        state.last_log_update = Some(Instant::now());
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    async fn update_container_stats(&self, container_id: &str) -> Result<()> {
        if let Some(docker) = &self.docker {
            let should_update = {
                self.containers.lock()
                    .ok()
                    .and_then(|containers| containers.get(container_id).cloned())
                    .and_then(|state| state.stats.last_update)
                    .map(|last| last.elapsed() >= Duration::from_secs(2))
                    .unwrap_or(true)
            };

            if should_update {
                let mut stats_stream = docker.stats(container_id, Some(StatsOptions {
                    stream: false,
                    ..Default::default()
                }));

                if let Some(Ok(stats)) = stats_stream.next().await {
                    let cpu_usage = calculate_cpu_percentage(&stats);
                    let memory_usage = stats.memory_stats.usage.unwrap_or(0);
                    let memory_limit = stats.memory_stats.limit.unwrap_or(0);
                    
                    let (network_rx, network_tx) = stats.networks.map(|networks| {
                        networks.values().fold((0, 0), |(rx, tx), net| {
                            (rx + net.rx_bytes, tx + net.tx_bytes)
                        })
                    }).unwrap_or((0, 0));

                    let block_read = stats.blkio_stats.io_service_bytes_recursive
                        .iter()
                        .flat_map(|s| s.iter())
                        .filter(|s| s.op.to_lowercase() == "read")
                        .map(|s| s.value)
                        .sum::<u64>();

                    let block_write = stats.blkio_stats.io_service_bytes_recursive
                        .iter()
                        .flat_map(|s| s.iter())
                        .filter(|s| s.op.to_lowercase() == "write")
                        .map(|s| s.value)
                        .sum::<u64>();

                    if let Ok(mut containers) = self.containers.lock() {
                        if let Some(state) = containers.get_mut(container_id) {
                            state.stats = ContainerStats {
                                cpu_usage,
                                memory_usage,
                                memory_limit,
                                network_rx,
                                network_tx,
                                block_read,
                                block_write,
                                last_update: Some(Instant::now()),
                            };
                        }
                    }
                } else {
                    eprintln!("Не удалось получить статистику для контейнера {}", container_id);
                }
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    async fn update_container_info(&self, container_id: &str) -> Result<()> {
        if let Some(docker) = &self.docker {
            if let Ok(info) = docker.inspect_container(container_id, None).await {
                let mut port_mappings = Vec::new();
                let mut container_info = ContainerInfo::default();
                
                // Обновляем информацию о портах
                if let Some(network_settings) = info.network_settings {
                    if let Some(ports) = network_settings.ports {
                        for (port_proto, bindings) in ports {
                            if let Some(bindings) = bindings {
                                for binding in bindings {
                                    if let Some(host_port) = binding.host_port {
                                        if let Some(container_port) = port_proto.split('/').next() {
                                            if let Ok(container_port) = container_port.parse::<u16>() {
                                                if let Ok(host_port) = host_port.parse::<u16>() {
                                                    port_mappings.push(PortMapping {
                                                        container_port,
                                                        host_port,
                                                        protocol: port_proto.split('/').nth(1)
                                                            .unwrap_or("tcp").to_string(),
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Собираем основную информацию о контейнере
                if let Some(config) = info.config {
                    if let Some(image) = config.image {
                        container_info.image = image;
                    }
                    if let Some(env) = config.env {
                        container_info.env = env;
                    }
                }

                // Время создания
                if let Some(created) = info.created {
                    if let Ok(dt) = DateTime::parse_from_rfc3339(&created) {
                        container_info.created = SystemTime::from(dt);
                    }
                }

                // Информация о томах
                if let Some(mounts) = info.mounts {
                    for mount in mounts {
                        if let (Some(source), Some(target)) = (mount.source, mount.destination) {
                            container_info.volumes.push((source, target));
                        }
                    }
                }

                if let Ok(mut containers) = self.containers.lock() {
                    if let Some(state) = containers.get_mut(container_id) {
                        state.ports = port_mappings;
                        state.info = container_info;
                    }
                }
            }
        }
        Ok(())
    }

    fn open_in_browser(&self, port: u16) {
        let url = format!("http://localhost:{}", port);
        #[cfg(target_os = "linux")]
        {
            if let Err(e) = Command::new("xdg-open").arg(&url).spawn() {
                eprintln!("Failed to open URL: {}", e);
            }
        }
        #[cfg(target_os = "macos")]
        {
            if let Err(e) = Command::new("open").arg(&url).spawn() {
                eprintln!("Failed to open URL: {}", e);
            }
        }
        #[cfg(target_os = "windows")]
        {
            if let Err(e) = Command::new("cmd").arg("/C").arg("start").arg(&url).spawn() {
                eprintln!("Failed to open URL: {}", e);
            }
        }
    }

    fn open_terminal(&self, container_id: &str) {
        // Определяем команду для терминала в зависимости от окружения
        let terminal_cmd = if cfg!(target_os = "linux") {
            // Проверяем наличие различных терминалов
            if Command::new("gnome-terminal").arg("--version").output().is_ok() {
                Some(("gnome-terminal", vec!["--"]))
            } else if Command::new("konsole").arg("--version").output().is_ok() {
                Some(("konsole", vec!["--separate", "--"]))
            } else if Command::new("xfce4-terminal").arg("--version").output().is_ok() {
                Some(("xfce4-terminal", vec!["--"]))
            } else {
                None
            }
        } else if cfg!(target_os = "macos") {
            Some(("open", vec!["-a", "Terminal", "--"]))
        } else if cfg!(target_os = "windows") {
            Some(("cmd", vec!["/C", "start"]))
        } else {
            None
        };

        if let Some((term, args)) = terminal_cmd {
            let mut command = Command::new(term);
            command.args(args);
            
            // Добавляем команду docker exec
            let docker_args = vec![
                "docker",
                "exec",
                "-it",
                container_id,
                "/bin/sh",
                "-c",
                "if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi"
            ];
            command.args(docker_args);

            if let Err(e) = command.spawn() {
                eprintln!("Failed to open terminal: {}", e);
            }
        } else {
            eprintln!("No suitable terminal found");
        }
    }

    async fn check_shell_available(&self, container_id: &str) -> bool {
        if let Some(docker) = &self.docker {
            let options = CreateExecOptions {
                cmd: Some(vec!["sh", "-c", "command -v bash >/dev/null 2>&1 || command -v sh >/dev/null 2>&1"]),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            };

            match docker.create_exec(container_id, options).await {
                Ok(exec) => {
                    if (docker.start_exec(&exec.id, None).await).is_ok() {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
        false
    }
}

#[allow(dead_code)]
fn calculate_cpu_percentage(stats: &Stats) -> f64 {
    let cpu_delta = stats.cpu_stats.cpu_usage.total_usage as i64 - 
                    stats.precpu_stats.cpu_usage.total_usage as i64;
    let system_delta = stats.cpu_stats.system_cpu_usage.unwrap_or(0) as i64 - 
                      stats.precpu_stats.system_cpu_usage.unwrap_or(0) as i64;
    
    if cpu_delta > 0 && system_delta > 0 {
        let cpu_count = stats.cpu_stats.online_cpus.unwrap_or(1) as f64;
        (cpu_delta as f64 / system_delta as f64) * 100.0 * cpu_count
    } else {
        0.0
    }
}

#[allow(dead_code)]
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut bytes = bytes as f64;
    let mut unit = 0;

    while bytes >= 1024.0 && unit < UNITS.len() - 1 {
        bytes /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{:.0} {}", bytes, UNITS[unit])
    } else {
        format!("{:.2} {}", bytes, UNITS[unit])
    }
}

impl eframe::App for DockerGuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut visuals = egui::Visuals::dark();
        visuals.window_rounding = egui::Rounding {
            nw: 10.0,
            ne: 10.0,
            sw: 10.0,
            se: 10.0,
        };
        visuals.widgets.noninteractive.rounding = egui::Rounding {
            nw: 5.0,
            ne: 5.0,
            sw: 5.0,
            se: 5.0,
        };
        visuals.widgets.inactive.rounding = egui::Rounding {
            nw: 5.0,
            ne: 5.0,
            sw: 5.0,
            se: 5.0,
        };
        visuals.widgets.active.rounding = egui::Rounding {
            nw: 5.0,
            ne: 5.0,
            sw: 5.0,
            se: 5.0,
        };
        visuals.widgets.hovered.rounding = egui::Rounding {
            nw: 5.0,
            ne: 5.0,
            sw: 5.0,
            se: 5.0,
        };
        ctx.set_visuals(visuals);

        // Автоматическое обновление
        if self.compose_file.is_some() {
            self.update_containers_if_needed();
        }

        egui::SidePanel::left("file_browser")
            .min_width(250.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    ui.heading("📁 Файловый браузер");
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                
                if ui.add(egui::Button::new("⬆️ Вверх").min_size(egui::vec2(ui.available_width(), 30.0))).clicked() {
                    if let Some(parent) = self.current_path.parent() {
                        self.current_path = parent.to_path_buf();
                        self.check_compose_file();
                    }
                }

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label("Текущий путь:");
                ui.label(self.current_path.to_string_lossy().to_string());
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for entry in WalkDir::new(&self.current_path)
                        .min_depth(1)
                        .max_depth(1)
                        .into_iter()
                        .filter_map(|e| e.ok())
                    {
                        let path = entry.path();
                        let name = path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("???");
                        
                        let icon = if path.is_dir() { "📁" } else { "📄" };
                        let is_compose = path.ends_with("docker-compose.yml");
                        let button = egui::Button::new(format!("{} {}", icon, name))
                            .wrap(false)
                            .min_size(egui::vec2(ui.available_width(), 30.0))
                            .fill(if is_compose {
                                egui::Color32::from_rgb(0, 100, 0)
                            } else {
                                ui.visuals().widgets.inactive.bg_fill
                            });

                        ui.add_space(4.0);
                        if ui.add(button).clicked() && path.is_dir() {
                            self.current_path = path.to_path_buf();
                            self.check_compose_file();
                        }
                    }
                });
            });

        egui::TopBottomPanel::bottom("copyright")
            .min_height(40.0)
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(30, 30, 30)))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::centered_and_justified(egui::Direction::LeftToRight), |ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            ui.visuals_mut().override_text_color = Some(egui::Color32::from_rgb(180, 180, 180));
                            ui.label("© LeadsFlow Team");
                            ui.label("⚡");
                        });
                    });
                    ui.add_space(8.0);
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(egui::style::Margin::symmetric(0.0, 40.0)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                    .show(ui, |ui| {
                        ui.add_space(16.0);
                        ui.vertical_centered(|ui| {
                            ui.heading("🚀 SmartHorizoker");
                            ui.add_space(4.0);
                            ui.label("Умное управление Docker-контейнерами");
                        });
                        ui.add_space(16.0);
                        ui.separator();
                        ui.add_space(16.0);

                        if self.compose_file.is_some() {
                            ui.vertical_centered(|ui| {
                                ui.add_space(24.0);
                                ui.heading(egui::RichText::new("🐳 Docker Compose проект").size(24.0));
                                ui.add_space(24.0);
                            });

                            if let Ok(containers) = self.containers.lock() {
                                if containers.is_empty() {
                                    ui.vertical_centered(|ui| {
                                        ui.add_space(32.0);
                                        ui.label(egui::RichText::new("Нет запущенных контейнеров").size(16.0));
                                    });
                                } else {
                                    for (id, state) in containers.iter() {
                                        ui.add_space(12.0);
                                        let is_running = state.status.contains("Up");
                                        let status_color = if is_running {
                                            egui::Color32::from_rgb(50, 200, 100)
                                        } else {
                                            egui::Color32::from_rgb(200, 50, 50)
                                        };

                                        // Карточка контейнера
                                        egui::Frame::none()
                                            .fill(ui.visuals().extreme_bg_color)
                                            .rounding(egui::Rounding::same(12.0))
                                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(60)))
                                            .outer_margin(egui::style::Margin::same(4.0))
                                            .inner_margin(egui::style::Margin::same(12.0))
                                            .show(ui, |ui| {
                                                ui.vertical(|ui| {
                                                    // Верхняя панель
                                                    ui.horizontal(|ui| {
                                                        // Статус и название
                                                        ui.label(egui::RichText::new("●").color(status_color).size(16.0));
                                                        ui.add_space(8.0);
                                                        ui.heading(egui::RichText::new(&state.name).size(20.0).strong());
                                                        
                                                        // Кнопки управления справа
                                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                            // Кнопка разворачивания
                                                            if ui.button(if state.expanded { "🔼" } else { "🔽" }).clicked() {
                                                                if let Ok(mut containers) = self.containers.lock() {
                                                                    if let Some(container_state) = containers.get_mut(id) {
                                                                        container_state.expanded = !container_state.expanded;
                                                                    }
                                                                }
                                                            }

                                                            ui.add_space(8.0);

                                                            // Кнопки управления контейнером
                                                            if is_running {
                                                                if ui.button("⏹").clicked() {
                                                                    let id = id.clone();
                                                                    let app_copy = self.clone();
                                                                    tokio::spawn(async move {
                                                                        if let Err(e) = app_copy.stop_container(&id).await {
                                                                            eprintln!("Ошибка остановки контейнера: {}", e);
                                                                        }
                                                                    });
                                                                }
                                                                ui.add_space(4.0);
                                                                if ui.button("🔄").clicked() {
                                                                    let id = id.clone();
                                                                    let app_copy = self.clone();
                                                                    tokio::spawn(async move {
                                                                        if let Err(e) = app_copy.restart_container(&id).await {
                                                                            eprintln!("Ошибка перезапуска контейнера: {}", e);
                                                                        }
                                                                    });
                                                                }
                                                            } else if ui.button("▶").clicked() {
                                                                let id = id.clone();
                                                                let app_copy = self.clone();
                                                                tokio::spawn(async move {
                                                                    if let Err(e) = app_copy.start_container(&id).await {
                                                                        eprintln!("Ошибка запуска контейнера: {}", e);
                                                                    }
                                                                });
                                                            }
                                                        });
                                                    });

                                                    ui.add_space(8.0);

                                                    // Статус и статистика
                                                    ui.horizontal(|ui| {
                                                        ui.label(egui::RichText::new(&state.status).color(egui::Color32::from_gray(180)));
                                                        
                                                        if is_running {
                                                            ui.add_space(16.0);
                                                            
                                                            // CPU
                                                            {
                                                                ui.label("CPU:");
                                                                ui.add_space(4.0);
                                                                let progress = (state.stats.cpu_usage / 100.0) as f32;
                                                                let response = ui.add(
                                                                    egui::ProgressBar::new(progress.clamp(0.0, 1.0))
                                                                        .desired_width(100.0)
                                                                        .text(format!("{:.1}%", state.stats.cpu_usage))
                                                                );
                                                                if response.hovered() {
                                                                    response.on_hover_text("Использование CPU");
                                                                }
                                                            }

                                                            ui.add_space(16.0);

                                                            // RAM
                                                            {
                                                                ui.label("RAM:");
                                                                ui.add_space(4.0);
                                                                let memory_percent = if state.stats.memory_limit > 0 {
                                                                    state.stats.memory_usage as f64 / state.stats.memory_limit as f64 * 100.0
                                                                } else {
                                                                    0.0
                                                                };
                                                                let progress = (memory_percent / 100.0) as f32;
                                                                let response = ui.add(
                                                                    egui::ProgressBar::new(progress.clamp(0.0, 1.0))
                                                                        .desired_width(100.0)
                                                                        .text(format!("{:.1}%", memory_percent))
                                                                );
                                                                if response.hovered() {
                                                                    response.on_hover_text(format!(
                                                                        "Использовано {} из {}",
                                                                        format_bytes(state.stats.memory_usage),
                                                                        format_bytes(state.stats.memory_limit)
                                                                    ));
                                                                }
                                                            }
                                                        }
                                                    });

                                                    // Развернутая информация
                                                    if state.expanded {
                                                        ui.add_space(12.0);
                                                        ui.separator();
                                                        ui.add_space(12.0);

                                                        // Кнопки действий
                                                        ui.horizontal(|ui| {
                                                            let button_text_size = 14.0;
                                                            
                                                            if ui.button(
                                                                egui::RichText::new(
                                                                    if state.show_logs { "🔽 Скрыть логи" } else { "🔼 Показать логи" }
                                                                ).size(button_text_size)
                                                            ).clicked() {
                                                                if let Ok(mut containers) = self.containers.lock() {
                                                                    if let Some(state) = containers.get_mut(id) {
                                                                        state.show_logs = !state.show_logs;
                                                                        if state.show_logs {
                                                                            let id = id.clone();
                                                                            let app_copy = self.clone();
                                                                            tokio::spawn(async move {
                                                                                if let Err(e) = app_copy.update_container_logs(&id).await {
                                                                                    eprintln!("Error updating logs: {}", e);
                                                                                }
                                                                            });
                                                                        }
                                                                    }
                                                                }
                                                            }

                                                            ui.add_space(8.0);

                                                            if !state.ports.is_empty() {
                                                                if ui.button(
                                                                    egui::RichText::new(
                                                                        if state.show_ports { "🔽 Скрыть порты" } else { "🔼 Показать порты" }
                                                                    ).size(button_text_size)
                                                                ).clicked() {
                                                                    if let Ok(mut containers) = self.containers.lock() {
                                                                        if let Some(state) = containers.get_mut(id) {
                                                                            state.show_ports = !state.show_ports;
                                                                        }
                                                                    }
                                                                }
                                                                ui.add_space(8.0);
                                                            }

                                                            if ui.button(
                                                                egui::RichText::new(
                                                                    if state.show_info { "🔽 Скрыть информацию" } else { "🔼 Показать информацию" }
                                                                ).size(button_text_size)
                                                            ).clicked() {
                                                                if let Ok(mut containers) = self.containers.lock() {
                                                                    if let Some(state) = containers.get_mut(id) {
                                                                        state.show_info = !state.show_info;
                                                                    }
                                                                }
                                                            }

                                                            if is_running {
                                                                ui.add_space(8.0);
                                                                if ui.button(egui::RichText::new("🖥️ Терминал").size(button_text_size)).clicked() {
                                                                    let id = id.clone();
                                                                    let app_copy = self.clone();
                                                                    tokio::spawn(async move {
                                                                        if app_copy.check_shell_available(&id).await {
                                                                            app_copy.open_terminal(&id);
                                                                        }
                                                                    });
                                                                }
                                                            }
                                                        });

                                                        // Показываем дополнительную информацию
                                                        if state.show_logs {
                                                            ui.add_space(12.0);
                                                            egui::Frame::none()
                                                                .fill(ui.visuals().extreme_bg_color)
                                                                .rounding(egui::Rounding::same(6.0))
                                                                .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(60)))
                                                                .inner_margin(egui::style::Margin::same(8.0))
                                                                .show(ui, |ui| {
                                                                    let text_style = egui::TextStyle::Monospace;
                                                                    let row_height = ui.text_style_height(&text_style);
                                                                    egui::ScrollArea::vertical()
                                                                        .max_height(200.0)
                                                                        .show_rows(ui, row_height,
                                                                            state.logs.lines().count() + 1,
                                                                            |ui, _| {
                                                                                ui.add(
                                                                                    egui::TextEdit::multiline(&mut state.logs.as_str())
                                                                                        .desired_width(f32::INFINITY)
                                                                                        .font(text_style)
                                                                                        .interactive(false)
                                                                                );
                                                                            }
                                                                        );
                                                                });
                                                        }

                                                        if state.show_ports && !state.ports.is_empty() {
                                                            ui.add_space(12.0);
                                                            egui::Frame::none()
                                                                .fill(ui.visuals().extreme_bg_color)
                                                                .rounding(egui::Rounding::same(6.0))
                                                                .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(60)))
                                                                .inner_margin(egui::style::Margin::same(8.0))
                                                                .show(ui, |ui| {
                                                                    egui::Grid::new("ports_grid")
                                                                        .spacing(egui::vec2(16.0, 8.0))
                                                                        .show(ui, |ui| {
                                                                            ui.label(egui::RichText::new("Контейнер").strong());
                                                                            ui.label(egui::RichText::new("Хост").strong());
                                                                            ui.label(egui::RichText::new("Протокол").strong());
                                                                            ui.label("");
                                                                            ui.end_row();

                                                                            for port in &state.ports {
                                                                                ui.label(format!("{}", port.container_port));
                                                                                ui.label(format!("{}", port.host_port));
                                                                                ui.label(&port.protocol);
                                                                                
                                                                                if port.protocol == "tcp" {
                                                                                    if ui.button("🌐").clicked() {
                                                                                        self.open_in_browser(port.host_port);
                                                                                    }
                                                                                } else {
                                                                                    ui.label("");
                                                                                }
                                                                                ui.end_row();
                                                                            }
                                                                        });
                                                                });
                                                        }

                                                        if state.show_info {
                                                            ui.add_space(12.0);
                                                            egui::Frame::none()
                                                                .fill(ui.visuals().extreme_bg_color)
                                                                .rounding(egui::Rounding::same(6.0))
                                                                .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(60)))
                                                                .inner_margin(egui::style::Margin::same(8.0))
                                                                .show(ui, |ui| {
                                                                    // Образ
                                                                    ui.label(egui::RichText::new("Образ:").strong());
                                                                    ui.monospace(&state.info.image);
                                                                    ui.add_space(8.0);

                                                                    // Время создания
                                                                    ui.label(egui::RichText::new("Создан:").strong());
                                                                    let created: DateTime<Local> = state.info.created.into();
                                                                    ui.label(created.format("%Y-%m-%d %H:%M:%S").to_string());
                                                                    ui.add_space(8.0);

                                                                    // Переменные окружения
                                                                    if !state.info.env.is_empty() {
                                                                        ui.label(egui::RichText::new("Переменные окружения:").strong());
                                                                        ui.add_space(4.0);
                                                                        egui::ScrollArea::vertical()
                                                                            .max_height(100.0)
                                                                            .show(ui, |ui| {
                                                                                for env in &state.info.env {
                                                                                    ui.monospace(env);
                                                                                }
                                                                            });
                                                                        ui.add_space(8.0);
                                                                    }

                                                                    // Тома
                                                                    if !state.info.volumes.is_empty() {
                                                                        ui.label(egui::RichText::new("Подключенные тома:").strong());
                                                                        ui.add_space(4.0);
                                                                        egui::ScrollArea::vertical()
                                                                            .max_height(100.0)
                                                                            .show(ui, |ui| {
                                                                                for (host, container) in &state.info.volumes {
                                                                                    ui.horizontal(|ui| {
                                                                                        ui.monospace(host);
                                                                                        ui.label("→");
                                                                                        ui.monospace(container);
                                                                                    });
                                                                                }
                                                                            });
                                                                    }
                                                                });
                                                        }
                                                    }
                                                });
                                            });
                                    }
                                    ui.add_space(12.0);
                                }
                            }
                        } else {
                            ui.vertical_centered(|ui| {
                                ui.add_space(32.0);
                                ui.heading("👈 Выберите директорию с docker-compose.yml");
                                ui.add_space(16.0);
                                ui.label("Используйте файловый браузер слева для навигации");
                            });
                        }
                    });
            });

        ctx.request_repaint();
    }
}

#[tokio::main]
async fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 768.0])
            .with_min_inner_size([800.0, 600.0])
            .with_maximized(true)
            .with_title("SmartHorizoker")
            .with_resizable(true),
        follow_system_theme: true,
        default_theme: eframe::Theme::Dark,
        ..Default::default()
    };
    
    eframe::run_native(
        "SmartHorizoker",
        options,
        Box::new(|cc| Box::new(DockerGuiApp::new(cc)))
    )
} 