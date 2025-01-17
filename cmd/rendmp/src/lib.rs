// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use humility::core::Core;
use humility::hubris::*;
use humility_cmd::hiffy::*;
use humility_cmd::i2c::I2cArgs;
use humility_cmd::{Archive, Args, Attach, Command, Validate};

use anyhow::{bail, Result};
use clap::Command as ClapCommand;
use clap::{CommandFactory, Parser};
use hif::*;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::prelude::*;
use std::io::BufReader;
use std::io::Write;

#[derive(Parser, Debug)]
#[clap(name = "rendmp", about = env!("CARGO_PKG_DESCRIPTION"))]
struct RendmpArgs {
    /// sets timeout
    #[clap(
        long, short, default_value = "5000", value_name = "timeout_ms",
        parse(try_from_str = parse_int::parse)
    )]
    timeout: u32,

    /// specifies an I2C bus by name
    #[clap(long, short, value_name = "bus",
        conflicts_with_all = &["port", "controller"]
    )]
    bus: Option<String>,

    /// specifies an I2C controller
    #[clap(long, short, value_name = "controller",
        parse(try_from_str = parse_int::parse),
    )]
    controller: Option<u8>,

    /// specifies an I2C controller port
    #[clap(long, short, value_name = "port")]
    port: Option<String>,

    /// specifies I2C multiplexer and segment
    #[clap(long, short, value_name = "mux:segment")]
    mux: Option<String>,

    /// specifies an I2C device address
    #[clap(long, short = 'd', value_name = "address")]
    device: Option<String>,

    /// specifies a device by rail name
    #[clap(long, short = 'r', value_name = "rail")]
    rail: Option<String>,

    /// specifies a PMBus driver
    #[clap(long, short = 'D')]
    driver: Option<String>,

    /// dump all device memory
    #[clap(long)]
    dump: bool,

    /// ingest a Power Navigator text file
    #[clap(
        long,
        short = 'i',
        value_name = "filename",
        conflicts_with_all = &["bus", "device"],
    )]
    ingest: Option<String>,
}

fn all_commands(
    device: pmbus::Device,
) -> HashMap<String, (u8, pmbus::Operation, pmbus::Operation)> {
    let mut all = HashMap::new();

    for i in 0..=255u8 {
        device.command(i, |cmd| {
            all.insert(
                cmd.name().to_string(),
                (i, cmd.read_op(), cmd.write_op()),
            );
        });
    }

    all
}

#[derive(Copy, Clone, Debug)]
enum Address<'a> {
    Dma(u16),
    Pmbus(u8, &'a str),
}

struct Packet<'a> {
    address: Address<'a>,
    payload: Vec<u8>,
}

fn rendmp_gen(
    _subargs: &RendmpArgs,
    device: &pmbus::Device,
    packets: &[Packet],
    commands: &HashMap<String, (u8, pmbus::Operation, pmbus::Operation)>,
) -> Result<()> {
    println!(
        r##"// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

///
/// Iterate over a configuration payload for a Renesas {} digital multiphase
/// PWM controller.  This code was generated by "humility rendmp -g" given
/// a .txt dump from running Renesas configuration software.
///
#[rustfmt::skip]
pub fn {}_payload<E>(
    mut func: impl FnMut(&[u8]) -> Result<(), E>
) -> Result<(), E> {{

    const PAYLOAD: &[&[u8]] = &["##,
        device.name(),
        device.name(),
    );

    let dmaaddr = match commands.get("DMAADDR") {
        Some((code, _, write)) => {
            if *write != pmbus::Operation::WriteWord {
                bail!("DMAADDR mismatch: found {:?}", write);
            }
            *code
        }
        _ => {
            bail!("no DMAADDR command found; is this a Renesas device?");
        }
    };

    let dmafix = match commands.get("DMAFIX") {
        Some((code, _, write)) => {
            if *write != pmbus::Operation::WriteWord32 {
                bail!("DMADATA mismatch: found {:?}", write);
            }
            *code
        }
        _ => {
            bail!("no DMAFIX command found; is this a Renesas device?");
        }
    };

    for packet in packets {
        match packet.address {
            Address::Dma(addr) => {
                let p = addr.to_le_bytes();

                println!("        // DMAADDR = 0x{:04x}", addr);
                println!(
                    "        &[ 0x{:02x}, 0x{:02x}, 0x{:02x} ],\n",
                    dmaaddr, p[0], p[1]
                );

                println!("        // DMAFIX = {:x?}", packet.payload);
                print!("        &[ 0x{:02x}, ", dmafix);
            }

            Address::Pmbus(code, name) => {
                println!("        // {} = {:x?}", name, packet.payload);

                print!("        &[ 0x{:02x}, ", code);
            }
        }

        for byte in &packet.payload {
            print!("0x{:02x}, ", byte);
        }

        println!("],\n");
    }

    println!(
        r##"    ];

    for chunk in PAYLOAD {{
        func(chunk)?;
    }}

    Ok(())
}}"##
    );

    Ok(())
}

fn rendmp_ingest(subargs: &RendmpArgs) -> Result<()> {
    let filename = subargs.ingest.as_ref().unwrap();
    let file = fs::File::open(filename)?;
    let lines = BufReader::new(file).lines();

    let mut allcmds = HashMap::new();
    let mut packets = vec![];

    let device = if let Some(driver) = &subargs.driver {
        match pmbus::Device::from_str(driver) {
            Some(device) => device,
            None => {
                bail!("unknown device \"{}\"", driver);
            }
        }
    } else {
        bail!("must specify device driver");
    };

    for code in 0..0xffu8 {
        device.command(code, |cmd| {
            allcmds.insert(code, cmd.name());
        });
    }

    for (ndx, line) in lines.enumerate() {
        let line = line?;
        let lineno = ndx + 1;

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let contents = line.split_whitespace().collect::<Vec<_>>();

        if contents.len() != 4 || contents[2] != "#" {
            bail!("malformed line {}", lineno);
        }

        let payload = contents[1];

        if !payload.starts_with("0x") {
            bail!("bad payload prefix on line {}: {}", lineno, payload);
        }

        let payload = match payload.len() {
            4 => match parse_int::parse::<u8>(payload) {
                Ok(val) => val.to_le_bytes().to_vec(),
                Err(_) => {
                    bail!("bad payload on line {}: {}", lineno, payload);
                }
            },

            6 => match parse_int::parse::<u16>(payload) {
                Ok(val) => val.to_le_bytes().to_vec(),
                Err(_) => {
                    bail!("bad payload on line {}: {}", lineno, payload);
                }
            },

            10 => match parse_int::parse::<u32>(payload) {
                Ok(val) => val.to_le_bytes().to_vec(),
                Err(_) => {
                    bail!("bad payload on line {}: {}", lineno, payload);
                }
            },

            _ => {
                bail!("badly sized payload on line {}: {}", lineno, payload);
            }
        };

        let address = contents[3];

        //
        // This is lame, but the only way to differentiate PMBus writes
        // (single-byte address) from DMA writes (dual-byte) is to look
        // at length of the string:
        //
        if !address.starts_with("0x") {
            bail!("bad address on line {}: {}", lineno, address);
        }

        let address = if address.len() > 4 {
            match parse_int::parse::<u16>(address) {
                Ok(dmaaddr) => Address::Dma(dmaaddr),
                Err(_) => {
                    bail!("bad DMA address on line {}: {}", lineno, address);
                }
            }
        } else {
            match parse_int::parse::<u8>(address) {
                Ok(paddr) => {
                    Address::Pmbus(paddr, allcmds.get(&paddr).unwrap())
                }
                Err(_) => {
                    bail!("bad PMBus address on line {}: {}", lineno, address);
                }
            }
        };

        packets.push(Packet { address, payload });
    }

    packets.push(Packet {
        address: Address::Pmbus(0xe7, allcmds.get(&0xe7).unwrap()),
        payload: vec![1, 0],
    });

    let commands = all_commands(device);
    rendmp_gen(subargs, &device, &packets, &commands)?;

    Ok(())
}

fn rendmp(
    hubris: &HubrisArchive,
    core: &mut dyn Core,
    _args: &Args,
    subargs: &[String],
) -> Result<()> {
    let subargs = RendmpArgs::try_parse_from(subargs)?;

    if subargs.ingest.is_some() {
        return rendmp_ingest(&subargs);
    }

    let mut context = HiffyContext::new(hubris, core, subargs.timeout)?;
    let funcs = context.functions()?;
    let i2c_read = funcs.get("I2cRead", 7)?;
    let i2c_write = funcs.get("I2cWrite", 8)?;

    let hargs = match (&subargs.rail, &subargs.device) {
        (Some(rail), None) => {
            let mut found = None;

            for device in &hubris.manifest.i2c_devices {
                if let HubrisI2cDeviceClass::Pmbus { rails } = &device.class {
                    for r in rails {
                        if rail == r {
                            found = match found {
                                Some(_) => {
                                    bail!("multiple devices match {}", rail);
                                }
                                None => Some(device),
                            }
                        }
                    }
                }
            }

            match found {
                None => {
                    bail!("rail {} not found", rail);
                }
                Some(device) => I2cArgs::from_device(device),
            }
        }

        (_, _) => I2cArgs::parse(
            hubris,
            &subargs.bus,
            subargs.controller,
            &subargs.port,
            &subargs.mux,
            &subargs.device,
        )?,
    };

    let device = if let Some(driver) = &subargs.driver {
        match pmbus::Device::from_str(driver) {
            Some(device) => device,
            None => {
                bail!("unknown device \"{}\"", driver);
            }
        }
    } else if let Some(driver) = hargs.device {
        match pmbus::Device::from_str(&driver) {
            Some(device) => device,
            None => pmbus::Device::Common,
        }
    } else {
        pmbus::Device::Common
    };

    let all = all_commands(device);

    let dmaaddr = match all.get("DMAADDR") {
        Some((code, _, write)) => {
            if *write != pmbus::Operation::WriteWord {
                bail!("DMAADDR mismatch: found {:?}", write);
            }
            *code
        }
        _ => {
            bail!("no DMAADDR command found; is this a Renesas device?");
        }
    };

    let dmaseq = match all.get("DMASEQ") {
        Some((code, read, _)) => {
            if *read != pmbus::Operation::ReadWord32 {
                bail!("DMASEQ mismatch: found {:?}", read);
            }
            *code
        }
        _ => {
            bail!("no DMASEQ command found; is this a Renesas device?");
        }
    };

    let mut base = vec![Op::Push(hargs.controller), Op::Push(hargs.port.index)];

    if let Some(mux) = hargs.mux {
        base.push(Op::Push(mux.0));
        base.push(Op::Push(mux.1));
    } else {
        base.push(Op::PushNone);
        base.push(Op::PushNone);
    }

    if let Some(address) = hargs.address {
        base.push(Op::Push(address));
    } else {
        bail!("expected device");
    }

    if subargs.dump {
        let blocksize = 128u8;
        let nblocks = 8;
        let memsize = 256 * 1024usize;
        let laps = memsize / (blocksize as usize * nblocks);
        let mut addr = 0;

        let bar = ProgressBar::new(memsize as u64);

        let mut filename;
        let mut i = 0;

        let filename = loop {
            filename = format!("hubris.rendmp.dump.{}", i);

            if let Ok(_f) = fs::File::open(&filename) {
                i += 1;
                continue;
            }

            break filename;
        };

        let mut file =
            OpenOptions::new().write(true).create_new(true).open(&filename)?;

        humility::msg!("dumping device memory to {}", filename);

        bar.set_style(ProgressStyle::default_bar().template(
            "humility: dumping device memory \
                          [{bar:30}] {bytes}/{total_bytes}",
        ));

        for lap in 0..laps {
            let mut ops = base.clone();

            //
            // If this is our first lap through, set our address to be 0
            //
            if lap == 0 {
                ops.push(Op::Push(dmaaddr));
                ops.push(Op::Push(0));
                ops.push(Op::Push(0));
                ops.push(Op::Push(2));
                ops.push(Op::Call(i2c_write.id));
                ops.push(Op::DropN(4));
            }

            ops.push(Op::Push(dmaseq));
            ops.push(Op::Push(blocksize));

            //
            // Unspeakably lazy, but also much less complicated:  we just
            // unroll our loop here.
            //
            for _ in 0..nblocks {
                ops.push(Op::Call(i2c_read.id));
            }

            //
            // Kick it off
            //
            ops.push(Op::Done);

            let results = context.run(core, ops.as_slice(), None)?;

            let start = if lap == 0 {
                match results[0] {
                    Err(err) => {
                        bail!(
                            "failed to set address: {}",
                            i2c_write.strerror(err)
                        )
                    }
                    Ok(_) => 1,
                }
            } else {
                0
            };

            for result in &results[start..] {
                match result {
                    Ok(val) => {
                        file.write_all(val)?;
                        addr += val.len();
                        bar.set_position(addr as u64);
                    }
                    Err(err) => {
                        bail!("{:?}", err);
                    }
                }
            }
        }
    }

    Ok(())
}

pub fn init() -> (Command, ClapCommand<'static>) {
    (
        Command::Attached {
            name: "rendmp",
            archive: Archive::Required,
            attach: Attach::LiveOnly,
            validate: Validate::Booted,
            run: rendmp,
        },
        RendmpArgs::command(),
    )
}
