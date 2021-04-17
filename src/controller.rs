use anyhow::{anyhow, Result};
use bitfield::bitfield;
use chrono::{DateTime, Utc};
use pid::Pid;
use rppal::{
    gpio::{Gpio, OutputPin},
    spi::{Bus, Mode, SlaveSelect as SpiSelect, Spi},
};
use serde::{Deserialize, Serialize};
use std::thread;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::time::{self, sleep, Duration};

const CS: [u8; 4] = [17, 27, 22, 5];
const RELAY: u8 = 6;

bitfield! {
    pub struct Max31855(u32);
    impl Debug;
    pub oc_fault, _: 0;
    pub scg_fault, _: 1;
    pub scv_fault, _: 2;
    pub internal_temp_raw, _: 15, 4;
    pub fault, _: 16;
    pub junction_temp_raw, _: 31, 18;
}

fn sign_extend(val: u32, size: usize) -> i32 {
    let shift = 32 - size;
    ((val << shift) as i32) >> shift
}

fn temp_convert(val: u32, size: usize, msb: f32) -> f32 {
    sign_extend(val, size) as f32 * msb
}

#[derive(Debug)]
struct TempReader {
    req_tx: Sender<()>,
    resp_rx: Receiver<Result<Vec<f32>>>,
}

fn sample_temps(cs_pins: &mut Vec<OutputPin>, spi: &mut Spi) -> Result<Vec<f32>> {
    let mut temps = Vec::new();
    for pin in cs_pins.iter_mut() {
        let mut data = [0u8; 4];
        pin.set_low();
        spi.read(&mut data)?;
        pin.set_high();
        let val = Max31855(u32::from_be_bytes(data));
        let junction_temp = temp_convert(val.junction_temp_raw(), 14, 0.25);
        temps.push(junction_temp);
    }

    Ok(temps)
}

impl TempReader {
    pub fn new(mut cs_pins: Vec<OutputPin>, mut spi: Spi) -> TempReader {
        let (req_tx, mut req_rx) = channel::<()>(1);
        let (resp_tx, resp_rx) = channel::<Result<Vec<f32>>>(1);

        thread::spawn(move || loop {
            match req_rx.blocking_recv() {
                Some(_) => {
                    resp_tx
                        .blocking_send(sample_temps(&mut cs_pins, &mut spi))
                        .unwrap();
                }
                None => break,
            };
        });

        TempReader { req_tx, resp_rx }
    }

    pub async fn read(&mut self) -> Result<Vec<f32>> {
        self.req_tx.send(()).await?;
        self.resp_rx
            .recv()
            .await
            .ok_or(anyhow!("response channel closed"))?
    }
}

#[derive(Debug)]
struct OutputTask {
    tick_tx: Sender<f32>,
}

impl OutputTask {
    pub fn new(mut relay_pin: OutputPin, period: Duration) -> OutputTask {
        let (tick_tx, mut tick_rx) = channel::<f32>(1);

        tokio::spawn(async move {
            loop {
                let duty = tick_rx.recv().await.unwrap();
                if duty > 0.0 {
                    relay_pin.set_high();
                    sleep(period.mul_f32(duty)).await;
                    relay_pin.set_low();
                }
            }
        });

        OutputTask { tick_tx }
    }

    pub async fn tick(&mut self, duty: f32) -> Result<()> {
        self.tick_tx.send(duty).await?;

        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Config {
    pub p: f32,
    pub i: f32,
    pub d: f32,
    pub set_point: f32,
    pub tc_index: usize,
    pub period: Duration,
}

#[derive(Debug)]
pub struct Controller {
    reader: TempReader,
    output: OutputTask,
    config: Config,
    status: Status,
    pid: Pid<f32>,
}

#[derive(Debug)]
pub enum Command {
    SetSetPoint(f32),
    SetPGain(f32),
    SetIGain(f32),
    SetDGain(f32),
    SetTcIndex(usize),
}

#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    timestamp: DateTime<Utc>,
    values: Vec<f32>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Log {
    labels: Vec<String>,
    entries: Vec<LogEntry>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    pub temps: Vec<f32>,
    pub p_term: f32,
    pub i_term: f32,
    pub d_term: f32,
    pub output: f32,
    pub config: Config,
    pub log: Log,
}

#[derive(Debug)]
pub struct StatusReceiver {
    status_rx: Receiver<Status>,
}
impl StatusReceiver {
    pub async fn recv_status(&mut self) -> Result<Status> {
        let status = self
            .status_rx
            .recv()
            .await
            .ok_or(anyhow!("failed to receive status"))?;

        Ok(status)
    }
}

#[derive(Clone, Debug)]
pub struct CommandSender {
    cmd_tx: Sender<Command>,
}

impl CommandSender {
    pub async fn set_set_point(&self, set_point: f32) -> Result<()> {
        self.cmd_tx.send(Command::SetSetPoint(set_point)).await?;
        Ok(())
    }

    pub async fn set_p_gain(&self, gain: f32) -> Result<()> {
        self.cmd_tx.send(Command::SetPGain(gain)).await?;
        Ok(())
    }

    pub async fn set_i_gain(&self, gain: f32) -> Result<()> {
        self.cmd_tx.send(Command::SetIGain(gain)).await?;
        Ok(())
    }

    pub async fn set_d_gain(&self, gain: f32) -> Result<()> {
        self.cmd_tx.send(Command::SetDGain(gain)).await?;
        Ok(())
    }

    pub async fn set_tc_index(&self, index: usize) -> Result<()> {
        self.cmd_tx.send(Command::SetTcIndex(index)).await?;
        Ok(())
    }
}

impl Controller {
    pub fn new(config: Config) -> Result<Controller> {
        let relay_pin = Gpio::new()?.get(RELAY)?.into_output();
        let mut cs_pins = Vec::new();
        for pin in &CS {
            let output = Gpio::new()?.get(*pin)?.into_output();
            cs_pins.push(output);
        }
        let spi = Spi::new(Bus::Spi0, SpiSelect::Ss0, 1000000, Mode::Mode0).unwrap();

        // Make sure all chip selects are high.
        for pin in cs_pins.iter_mut() {
            pin.set_high();
        }

        let pid = Pid::new(
            config.p,
            config.i,
            config.d,
            100.0,
            100.0,
            100.0,
            100.0,
            config.set_point,
        );

        let reader = TempReader::new(cs_pins, spi);
        let output = OutputTask::new(relay_pin, config.period);
        let mut labels = vec!["output".to_string(), "set point".to_string()];
        for i in 0..CS.len() {
            labels.push(format!("TC {}", i));
        }
        let log = Log {
            labels,
            entries: Vec::new(),
        };

        let status = Status {
            temps: vec![0.0; CS.len()],
            p_term: 0.0,
            i_term: 0.0,
            d_term: 0.0,
            output: 0.0,
            config: config.clone(),
            log,
        };
        Ok(Controller {
            reader,
            output,
            config,
            status,
            pid,
        })
    }

    async fn tick(&mut self) -> Result<()> {
        self.status.temps = self.reader.read().await?;

        // Sync PID params before ticking.
        self.pid.kp = self.config.p;
        self.pid.ki = self.config.i;
        self.pid.kd = self.config.d;
        self.pid.setpoint = self.config.set_point;

        let output = self
            .pid
            .next_control_output(self.status.temps[self.config.tc_index]);
        let duty = if output.output < 0.0 {
            0.0
        } else {
            output.output / 100.0
        };

        let mut values = vec![duty * 100.0, self.config.set_point];
        for temp in &self.status.temps {
            if *temp < 2000.0 {
                values.push(*temp);
            } else {
                values.push(0.0);
            }
        }
        self.status.log.entries.push(LogEntry {
            timestamp: Utc::now(),
            values,
        });

        self.output.tick(duty).await?;

        self.status.p_term = output.p;
        self.status.i_term = output.i;
        self.status.d_term = output.d;
        self.status.output = duty;

        Ok(())
    }

    async fn handle_cmd(&mut self, cmd: Command) -> Result<()> {
        match cmd {
            Command::SetSetPoint(val) => self.config.set_point = val,
            Command::SetPGain(val) => self.config.p = val,
            Command::SetIGain(val) => self.config.i = val,
            Command::SetDGain(val) => self.config.d = val,
            Command::SetTcIndex(val) => {
                if val < CS.len() {
                    self.config.tc_index = val
                }
            }
        }

        self.status.config = self.config.clone();

        Ok(())
    }

    pub async fn run(
        &mut self,
        mut cmd_rx: Receiver<Command>,
        status_tx: Sender<Status>,
    ) -> Result<()> {
        let mut interval = time::interval(self.config.period);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.tick().await?;
                }
                cmd = cmd_rx.recv() => {
                    if let Some(cmd) = cmd {
                        self.handle_cmd(cmd).await?;
                    }
                }
            }
            status_tx.send(self.status.clone()).await?;
        }
    }

    pub fn start(mut self) -> Result<(CommandSender, StatusReceiver)> {
        let (cmd_tx, cmd_rx) = channel::<Command>(1);
        let (status_tx, status_rx) = channel::<Status>(1);
        tokio::spawn(async move {
            self.run(cmd_rx, status_tx).await.unwrap();
        });
        Ok((CommandSender { cmd_tx }, StatusReceiver { status_rx }))
    }
}
