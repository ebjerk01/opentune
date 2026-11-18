//! Command handlers, one module per area, and the dispatcher that routes a
//! parsed [`Command`] to them.
use crate::cli::{Command, ContainerAction};
use crate::context::CliContext;
use crate::{candump, haldex_cmd, obd_cmd, recipe_cmd, ts_cmd};
use anyhow::Result;

pub mod checksum;
pub mod container;
pub mod defs;
pub mod device;
pub mod engines;
pub mod formats;
pub mod image;
pub mod module_flash;
pub mod reference;

/// Run one parsed core subcommand.
pub fn dispatch(ctx: &CliContext, cmd: Command) -> Result<()> {
    let reg = ctx.registry();
    match cmd {
        Command::Drivers => device::drivers(ctx),
        Command::Info(conn) => device::info(ctx, conn),
        Command::Read {
            conn,
            addr,
            len,
            out,
            plan,
        } => device::read(ctx, conn, addr, len, out, plan),
        Command::Diff { a, b } => image::diff_files(a, b),
        Command::Checksum { action } => checksum::run(ctx, action),
        Command::Flash {
            conn,
            addr,
            data,
            min_mv,
            vault,
            resume,
            transcript,
            chunk_size,
            module,
        } => {
            if module.uds_module {
                return module_flash::run(
                    ctx,
                    module_flash::FlashCommon {
                        conn,
                        addr,
                        data,
                        min_mv,
                        vault,
                        resume,
                        transcript,
                    },
                    *module,
                );
            }
            let (Some(addr), Some(data)) = (addr, data) else {
                anyhow::bail!("flash needs --addr and --data (or --uds-module --container)");
            };
            device::flash(
                ctx, conn, addr, data, min_mv, vault, resume, transcript, chunk_size,
            )
        }
        Command::Dump {
            conn,
            addr,
            len,
            vault,
            plan,
        } => device::dump(ctx, conn, addr, len, vault, plan),
        Command::Findmaps {
            stock,
            tuned,
            single,
            gap,
            min_len,
            emit,
        } => image::findmaps(stock, tuned, single, gap, min_len, emit),
        Command::Ecudefs { action } => defs::ecudefs(action),
        Command::Identify {
            conn,
            ecudefs,
            catalog,
        } => device::identify(ctx, conn, ecudefs, catalog),
        Command::Db { action } => defs::db(action),
        Command::Dtc { code } => reference::dtc(code),
        Command::Vin { vin } => reference::vin(vin),
        Command::DecodePart { part } => reference::decode_part(part),
        Command::Compare {
            ecudefs,
            def,
            stock,
            tuned,
            map,
        } => image::compare(ecudefs, def, stock, tuned, map),
        Command::ImportXdf { file, into } => formats::import_xdf(file, into),
        Command::ImportEcuflash { file, into } => formats::import_ecuflash(file, into),
        Command::ImportOls {
            file,
            version,
            into,
        } => formats::import_ols(file, version, into),
        Command::Packs { dir } => defs::packs(dir),
        Command::ImportA2l { file, into, base } => formats::import_a2l(file, into, base),
        Command::Vault { dir, verify } => device::vault(dir, verify),
        Command::A2l2Xdf {
            file,
            out,
            base,
            only,
        } => formats::a2l2xdf(file, out, base, only),
        Command::ExportXdf { ecudefs, def, out } => formats::export_xdf(ecudefs, def, out),
        Command::ExportA2l { ecudefs, def, out } => formats::export_a2l(ecudefs, def, out),
        Command::Scan {
            dir,
            human,
            catalog,
            ecudefs,
        } => reference::scan(dir, human, catalog, ecudefs),
        Command::Container { action } => match action {
            ContainerAction::Unpack {
                input,
                outdir,
                frf_key,
                aes_key,
                aes_iv,
                xor_key,
                sgo_table,
                sgo_cipher,
            } => container::container_unpack(container::ContainerUnpackArgs {
                input,
                outdir,
                frf_key,
                aes_key,
                aes_iv,
                xor_key,
                sgo_table,
                sgo_cipher,
            }),
        },
        Command::Cbf { file, json, dtcs } => reference::cbf(file, json, dtcs),
        Command::Coding { action } => engines::run_coding(reg, action),
        Command::Adaptation {
            def,
            driver,
            store,
            timeout_ms,
            action,
        } => engines::run_adaptation(reg, &def, &driver, store.as_deref(), timeout_ms, action),
        Command::Autoscan {
            modules,
            driver,
            store,
            timeout_ms,
        } => engines::run_autoscan(reg, &modules, &driver, store.as_deref(), timeout_ms),
        Command::Service {
            script,
            driver,
            store,
            timeout_ms,
            seedkey,
            action,
        } => engines::run_service(
            ctx,
            &script,
            &driver,
            store.as_deref(),
            timeout_ms,
            seedkey.as_deref(),
            action,
        ),
        Command::Simulate {
            dids,
            require_security_write,
            requests,
        } => engines::run_simulate(&dids, require_security_write, &requests),
        Command::Obd { action } => obd_cmd::run(reg, action),
        Command::Log {
            def,
            driver,
            store,
            timeout_ms,
            rounds,
            interval_ms,
            out,
            fast,
            dynamic_did,
            max_record,
            fast_session,
            params,
        } => {
            let fast =
                engines::fast_log_config(fast, &dynamic_did, max_record, fast_session.as_deref())?;
            engines::run_log(
                reg,
                &def,
                &driver,
                store.as_deref(),
                timeout_ms,
                rounds,
                interval_ms,
                out.as_deref(),
                &params,
                fast,
            )
        }
        Command::Candump(args) => candump::run(reg, args),
        Command::Recipe { action } => recipe_cmd::run(ctx, action),
        Command::Haldex { action } => haldex_cmd::run(reg, action),
        Command::Ts { action } => ts_cmd::run(action),
    }
}
