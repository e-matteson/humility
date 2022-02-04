// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ## `humility sensors`
//!
//! `humility sensors` communicates with the `sensor` Hubris task via its
//! `Sensor` Idol interface to get sensor data.  If there is no `sensor` task
//! or if there are no sensors defined in the in Hurbis application
//! description, this command will not provide any meaningful output. To list
//! all available sensors, use `-l` (`--list`); to summarize sensor values,
//! use `-s` (`--summarize`).  To constrain sensors by type, use the `-t`
//! (`--types`) option; to constrain sensors by device, use the `-d`
//! (`--devices`) option.  Within each option, multiple specifications serve
//! as a logical OR (that is, (`-d raa229618,tmp117` would yield all sensors
//! from either device), but if both kinds of specifications are present, they
//! serve as a logical AND (e.g., `-t thermal -d raa229618,tmp117` would yield
//! all thermal sensors from either device).

use anyhow::{bail, Context, Result};
use clap::App;
use clap::IntoApp;
use clap::Parser;
use hif::*;
use humility::core::Core;
use humility::hubris::*;
use humility_cmd::hiffy::*;
use humility_cmd::idol;
use humility_cmd::{Archive, Args, Attach, Command, Validate};
use std::collections::HashSet;
use std::thread;
use std::time::Duration;

#[derive(Parser, Debug)]
#[clap(name = "sensors", about = env!("CARGO_PKG_DESCRIPTION"))]
struct SensorsArgs {
    /// sets timeout
    #[clap(
        long, short = 'T', default_value = "5000", value_name = "timeout_ms",
        parse(try_from_str = parse_int::parse)
    )]
    timeout: u32,

    /// list all sensors
    #[clap(long, short)]
    list: bool,

    /// summarize sensors
    #[clap(long, short, conflicts_with = "list")]
    summarize: bool,

    /// restrict sensors by type of sensor
    #[clap(long, short, value_name = "sensor type", use_delimiter = true)]
    types: Option<Vec<String>>,

    /// restrict sensors by device
    #[clap(long, short, value_name = "device", use_delimiter = true)]
    devices: Option<Vec<String>>,
}

fn sensors_list(
    hubris: &HubrisArchive,
    types: &Option<HashSet<HubrisSensorKind>>,
    devices: &Option<HashSet<String>>,
) -> Result<()> {
    println!(
        "{:2} {:<7} {:2} {:2} {:3} {:4} {:13} {:4}",
        "ID", "KIND", "C", "P", "MUX", "ADDR", "DEVICE", "NAME"
    );

    for (ndx, s) in hubris.manifest.sensors.iter().enumerate() {
        if let Some(types) = types {
            if types.get(&s.kind).is_none() {
                continue;
            }
        }

        let device = &hubris.manifest.i2c_devices[s.device];

        if let Some(devices) = devices {
            if devices.get(&device.device).is_none() {
                continue;
            }
        }

        let mux = match (device.mux, device.segment) {
            (Some(m), Some(s)) => format!("{}:{}", m, s),
            (None, None) => "-".to_string(),
            (_, _) => "?:?".to_string(),
        };

        println!(
            "{:2} {:7} {:2} {:2} {:3} 0x{:02x} {:13} {:<1}",
            ndx,
            s.kind.to_string(),
            device.controller,
            device.port.name,
            mux,
            device.address,
            device.device,
            s.name,
        );
    }

    Ok(())
}

fn sensors_summarize(
    hubris: &HubrisArchive,
    core: &mut dyn Core,
    context: &mut HiffyContext,
    types: &Option<HashSet<HubrisSensorKind>>,
    devices: &Option<HashSet<String>>,
) -> Result<()> {
    let mut ops = vec![];
    let funcs = context.functions()?;
    let op = idol::IdolOperation::new(hubris, "Sensor", "get", None)
        .context("is the 'sensor' task present?")?;

    let ok = hubris.lookup_basetype(op.ok)?;

    if ok.encoding != HubrisEncoding::Float {
        bail!("expected return value of read_sensors() to be a float");
    }

    if ok.size != 4 {
        bail!("expected return value of read_sensors() to be an f32");
    }

    if hubris.manifest.sensors.is_empty() {
        bail!("no sensors found");
    }

    let mut rvals = vec![];

    for (i, s) in hubris.manifest.sensors.iter().enumerate() {
        if let Some(types) = types {
            if types.get(&s.kind).is_none() {
                continue;
            }
        }

        if let Some(devices) = devices {
            let d = &hubris.manifest.i2c_devices[s.device];

            if devices.get(&d.device).is_none() {
                continue;
            }
        }

        rvals.push(s);

        let payload =
            op.payload(&[("id", idol::IdolArgument::Scalar(i as u64))])?;
        context.idol_call_ops(&funcs, &op, &payload, &mut ops)?;
    }

    ops.push(Op::Done);

    for r in rvals {
        print!(" {:>12}", r.name.to_uppercase());
    }

    println!();

    loop {
        let results = context.run(core, ops.as_slice(), None)?;

        let mut rval = vec![];

        for r in results {
            if let Ok(val) = r {
                rval.push(Some(f32::from_le_bytes(val[0..4].try_into()?)));
            } else {
                rval.push(None);
            }
        }

        for val in rval {
            if let Some(val) = val {
                print!(" {:>12.2}", val);
            } else {
                print!(" {:>12}", "-");
            }
        }

        println!();

        thread::sleep(Duration::from_millis(1000));
    }
}

fn sensors(
    hubris: &HubrisArchive,
    core: &mut dyn Core,
    _args: &Args,
    subargs: &[String],
) -> Result<()> {
    let subargs = SensorsArgs::try_parse_from(subargs)?;

    let types = if let Some(types) = subargs.types {
        let mut rval = HashSet::new();

        for t in types {
            match HubrisSensorKind::from_string(&t) {
                Some(kind) => {
                    rval.insert(kind);
                }
                None => {
                    bail!("unrecognized sensor kind \"{}\"", t);
                }
            }
        }

        Some(rval)
    } else {
        None
    };

    let devices = if let Some(devices) = subargs.devices {
        let mut rval = HashSet::new();
        let mut all = HashSet::new();

        for d in hubris.manifest.i2c_devices.iter() {
            all.insert(&d.device);
        }

        for d in devices {
            match all.get(&d) {
                Some(_) => {
                    rval.insert(d);
                }
                None => {
                    bail!("unrecognized device {}", d);
                }
            }
        }

        Some(rval)
    } else {
        None
    };

    if subargs.list {
        sensors_list(hubris, &types, &devices)?;
        return Ok(());
    }

    let mut context = HiffyContext::new(hubris, core, subargs.timeout)?;

    if subargs.summarize {
        sensors_summarize(hubris, core, &mut context, &types, &devices)?;
        return Ok(());
    }

    Ok(())
}

pub fn init() -> (Command, App<'static>) {
    (
        Command::Attached {
            name: "sensors",
            archive: Archive::Required,
            attach: Attach::LiveOnly,
            validate: Validate::Booted,
            run: sensors,
        },
        SensorsArgs::into_app(),
    )
}