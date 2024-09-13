// Licensed under the Apache-2.0 license.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::rc::Rc;
use std::{error::Error, path::Path, process::Command};

use quote::__private::TokenStream;
use quote::{format_ident, quote};
use ureg_schema::{Enum, EnumVariant, Register, RegisterBlock, RegisterBlockInstance};

static HEADER_PREFIX: &str = r"/*
Licensed under the Apache-2.0 license.
";

static HEADER_SUFFIX: &str = r"
*/
";

static CALIPTRA_RDL_FILES: &[&str] = &[
    "src/pcrvault/rtl/pv_def.rdl",
    "src/pcrvault/rtl/pv_reg.rdl",
    "src/datavault/rtl/dv_reg.rdl",
    "src/libs/rtl/interrupt_regs.rdl",
    "src/keyvault/rtl/kv_def.rdl",
    "src/keyvault/rtl/kv_reg.rdl",
    "src/doe/rtl/doe_reg.rdl",
    "src/ecc/rtl/ecc_reg.rdl",
    "src/hmac/rtl/hmac_reg.rdl",
    "src/csrng/data/csrng.rdl",
    "src/entropy_src/data/entropy_src.rdl",
    "src/sha256/rtl/sha256_reg.rdl",
    "src/sha512/rtl/sha512_reg.rdl",
    "src/spi_host/data/spi_host.rdl",
    "src/soc_ifc/rtl/mbox_csr.rdl",
    "src/soc_ifc/rtl/soc_ifc_reg.rdl",
    "src/soc_ifc/rtl/sha512_acc_csr.rdl",
    "src/uart/data/uart.rdl",
    "src/integration/rtl/caliptra_reg.rdl",
];

static CALIPTRA_EXTRA_RDL_FILES: &[&str] =
    &["el2_pic_ctrl.rdl", "i3c-core/src/rdl/registers.rdl"];

fn run_cmd_stdout(cmd: &mut Command, input: Option<&[u8]>) -> Result<String, Box<dyn Error>> {
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());

    let mut child = cmd.spawn()?;
    if let (Some(mut stdin), Some(input)) = (child.stdin.take(), input) {
        std::io::Write::write_all(&mut stdin, input)?;
    }
    let out = child.wait_with_output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into())
    } else {
        Err(format!(
            "Process {:?} {:?} exited with status code {:?} stderr {}",
            cmd.get_program(),
            cmd.get_args(),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        )
        .into())
    }
}

fn remove_reg_prefixes(registers: &mut [Rc<Register>], prefix: &str) {
    for reg in registers.iter_mut() {
        if reg.name.to_ascii_lowercase().starts_with(prefix) {
            let reg = Rc::make_mut(reg);
            reg.name = reg.name[prefix.len()..].to_string();
        }
    }
}

fn rustfmt(code: &str) -> Result<String, Box<dyn Error>> {
    run_cmd_stdout(
        Command::new("rustfmt")
            .arg("--emit=stdout")
            .arg("--config=normalize_comments=true,normalize_doc_attributes=true"),
        Some(code.as_bytes()),
    )
}

fn write_file(dest_file: &Path, contents: &str) -> Result<(), Box<dyn Error>> {
    println!("Writing to {dest_file:?}");
    std::fs::write(dest_file, contents)?;
    Ok(())
}

fn file_check_contents(dest_file: &Path, expected_contents: &str) -> Result<(), Box<dyn Error>> {
    println!("Checking file {dest_file:?}");
    let actual_contents = std::fs::read(dest_file)?;
    if actual_contents != expected_contents.as_bytes() {
        return Err(format!(
            "{dest_file:?} does not match the generator output. If this is \
            unexpected, ensure that the caliptra-rtl submodule is pointing to \
            the correct commit and/or run \"git submodule update\". Otherwise, \
            run registers/update.sh to update this file."
        )
        .into());
    }
    Ok(())
}

fn real_main() -> Result<(), Box<dyn Error>> {
    let mut args: Vec<String> = std::env::args().collect();
    let file_action = if args.get(1).map(String::as_str) == Some("--check") {
        args.remove(1);
        file_check_contents
    } else {
        write_file
    };

    if args.len() < 4 {
        Err("Usage: codegen [--check] <caliptra_rtl_dir> <extra_rdl_dir> <dest_dir>")?;
    }

    let rtl_dir = Path::new(&args[1]);
    let mut rdl_files: Vec<PathBuf> = CALIPTRA_RDL_FILES
        .iter()
        .map(|p| rtl_dir.join(p))
        .filter(|p| p.exists())
        .collect();

    let extra_rdl_dir = Path::new(&args[2]);
    let mut extra_rdl_files: Vec<PathBuf> = CALIPTRA_EXTRA_RDL_FILES
        .iter()
        .map(|p| extra_rdl_dir.join(p))
        .filter(|p| p.exists())
        .collect();
    rdl_files.append(&mut extra_rdl_files);

    let rtl_commit_id = run_cmd_stdout(
        Command::new("git")
            .current_dir(rtl_dir)
            .arg("rev-parse")
            .arg("HEAD"),
        None,
    )?;
    let rtl_git_status = run_cmd_stdout(
        Command::new("git")
            .current_dir(rtl_dir)
            .arg("status")
            .arg("--porcelain"),
        None,
    )?;
    let mut header = HEADER_PREFIX.to_string();
    write!(
        &mut header,
        "\n generated by caliptra_registers_generator with caliptra-rtl repo at {rtl_commit_id}"
    )?;
    if !rtl_git_status.is_empty() {
        write!(
            &mut header,
            "\n\nWarning: rtl-caliptra was dirty:{rtl_git_status}"
        )?;
    }
    header.push_str(HEADER_SUFFIX);

    let dest_dir = Path::new(&args[args.len() - 1]);

    let file_source = caliptra_systemrdl::FsFileSource::new();
    let scope = caliptra_systemrdl::Scope::parse_root(&file_source, &rdl_files)
        .map_err(|s| s.to_string())?;
    let scope = scope.as_parent();

    let addrmap = scope.lookup_typedef("clp").unwrap();
    let addrmap2 = scope.lookup_typedef("clp2").unwrap();

    // These are types like kv_read_ctrl_reg that are used by multiple crates
    let root_block = RegisterBlock {
        declared_register_types: ureg_systemrdl::translate_types(scope)?,
        ..Default::default()
    };
    let mut root_block = root_block.validate_and_dedup()?;

    let mut extern_types = HashMap::new();
    ureg_codegen::build_extern_types(&root_block, quote! { crate }, &mut extern_types);

    let mut blocks = ureg_systemrdl::translate_addrmap(addrmap)?;
    let mut blocks2 = ureg_systemrdl::translate_addrmap(addrmap2)?;
    blocks.append(&mut blocks2);

    let mut validated_blocks = vec![];
    for mut block in blocks {
        if block.name.ends_with("_reg") || block.name.ends_with("_csr") {
            block.name = block.name[0..block.name.len() - 4].to_string();
        }
        if block.name == "hmac" {
            remove_reg_prefixes(&mut block.registers, "hmac384_");
        } else {
            remove_reg_prefixes(
                &mut block.registers,
                &format!("{}_", block.name.to_ascii_lowercase()),
            );
        }
        if block.name == "soc_ifc" {
            block.rename_enum_variants(&[
                ("DEVICE_UNPROVISIONED", "UNPROVISIONED"),
                ("DEVICE_MANUFACTURING", "MANUFACTURING"),
                ("DEVICE_PRODUCTION", "PRODUCTION"),
            ]);
            // Move the TRNG retrieval registers into an independent block;
            // these need to be owned by a separate driver than the rest of
            // soc_ifc.
            let mut trng_block = RegisterBlock {
                name: "soc_ifc_trng".into(),
                instances: vec![RegisterBlockInstance {
                    name: "soc_ifc_trng_reg".into(),
                    address: block.instances[0].address,
                }],
                ..Default::default()
            };
            block.registers.retain(|field| {
                if matches!(field.name.as_str(), "CPTRA_TRNG_DATA" | "CPTRA_TRNG_STATUS") {
                    trng_block.registers.push(field.clone());
                    false // remove field from soc_ifc
                } else {
                    true // keep field
                }
            });
            let trng_block = trng_block.validate_and_dedup()?;
            validated_blocks.push(trng_block);
        }

        let mut block = block.validate_and_dedup()?;

        if block.block().name == "ecc" {
            block.transform(|t| {
                // [TODO]: Put this enumeration into the RDL and remove this hack
                t.set_register_enum(
                    "CTRL",
                    "CTRL",
                    Rc::new(Enum {
                        name: Some("Ctrl".into()),
                        variants: vec![
                            EnumVariant {
                                name: "NONE".into(),
                                value: 0,
                            },
                            EnumVariant {
                                name: "KEYGEN".into(),
                                value: 1,
                            },
                            EnumVariant {
                                name: "SIGNING".into(),
                                value: 2,
                            },
                            EnumVariant {
                                name: "VERIFYING".into(),
                                value: 3,
                            },
                        ],
                        bit_width: 2,
                    }),
                );
            });
        }

        let module_ident = format_ident!("{}", block.block().name);
        ureg_codegen::build_extern_types(
            &block,
            quote! { crate::#module_ident },
            &mut extern_types,
        );
        validated_blocks.push(block);
    }
    let mut root_submod_tokens = TokenStream::new();

    let mut all_blocks: Vec<_> = std::iter::once(&mut root_block)
        .chain(validated_blocks.iter_mut())
        .collect();
    ureg_schema::filter_unused_types(&mut all_blocks);

    for block in validated_blocks {
        let module_ident = format_ident!("{}", block.block().name);
        let dest_file = dest_dir.join(format!("{}.rs", block.block().name));

        let tokens = ureg_codegen::generate_code(
            &block,
            ureg_codegen::Options {
                extern_types: extern_types.clone(),
                module: quote! { #module_ident },
            },
        );
        root_submod_tokens.extend(quote! { pub mod #module_ident; });
        file_action(
            &dest_file,
            &rustfmt(&(header.clone() + &tokens.to_string()))?,
        )?;
    }
    let root_type_tokens = ureg_codegen::generate_code(
        &root_block,
        ureg_codegen::Options {
            extern_types: extern_types.clone(),
            ..Default::default()
        },
    );
    let root_tokens = quote! { #root_type_tokens #root_submod_tokens };
    file_action(
        &dest_dir.join("lib.rs"),
        &rustfmt(&(header.clone() + &root_tokens.to_string()))?,
    )?;
    Ok(())
}

fn main() {
    if let Err(err) = real_main() {
        eprintln!("{}", err);
        std::process::exit(1);
    }
}
