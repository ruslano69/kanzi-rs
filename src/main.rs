mod alias;
mod ans;
mod binary_entropy;
mod bitio;
mod bwt;
mod cm;
mod container;
mod datatype;
mod exe;
mod fpaq;
mod fsd;
mod huffman_dec;
mod huffman_enc;
mod logtables;
mod lzp;
mod lzx;
mod magic;
mod rlt;
mod rolz;
mod sbrt;
mod srt;
mod text_codec;
mod text_codec1;
mod tpaq;
mod utf;
mod zrlt;

use bitio::BitWriter;
use huffman_dec::HuffmanDecoderV6;
use huffman_enc::HuffmanEncoder;
use lzx::LzxCodec;
use std::env;
use std::fs;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("usage:");
        eprintln!("  rust_kanzi lzxtest <fixture-prefix>          (expects <prefix>.orig/.fwd)");
        eprintln!("  rust_kanzi encode1 <input> <output.knz> [blockSize]");
        std::process::exit(1);
    }

    match args[1].as_str() {
        "lzxtest" => lzx_test(&args[2]),
        "encode1" => {
            let block_size: u32 = args
                .get(4)
                .map(|s| s.parse().unwrap())
                .unwrap_or(4 * 1024 * 1024);
            let data = fs::read(&args[2]).expect("read input");
            let out = container::encode_level1(&data, block_size);
            fs::write(&args[3], &out).expect("write output");
            println!(
                "encoded {} -> {} bytes ({:.2}%)",
                data.len(),
                out.len(),
                100.0 * out.len() as f64 / data.len() as f64
            );
        }
        "encode2" => {
            let block_size: u32 = args
                .get(4)
                .map(|s| s.parse().unwrap())
                .unwrap_or(4 * 1024 * 1024);
            let data = fs::read(&args[2]).expect("read input");
            let out = container::encode_level2(&data, block_size);
            fs::write(&args[3], &out).expect("write output");
            println!(
                "encoded {} -> {} bytes ({:.2}%)",
                data.len(),
                out.len(),
                100.0 * out.len() as f64 / data.len() as f64
            );
        }
        "huftest" => {
            let data = fs::read(&args[2]).expect("read input");
            let mut enc = HuffmanEncoder::new();
            let mut bw = BitWriter::new();
            enc.write(&data, &mut bw);
            let bytes = bw.finish();
            fs::write(&args[3], &bytes).expect("write output");
            println!(
                "huffman-encoded {} -> {} bytes ({:.2}%)",
                data.len(),
                bytes.len(),
                100.0 * bytes.len() as f64 / data.len() as f64
            );
        }
        "encode3" => {
            let block_size: u32 = args
                .get(4)
                .map(|s| s.parse().unwrap())
                .unwrap_or(4 * 1024 * 1024);
            let data = fs::read(&args[2]).expect("read input");
            let out = container::encode_level3(&data, block_size);
            fs::write(&args[3], &out).expect("write output");
            println!(
                "encoded {} -> {} bytes ({:.2}%)",
                data.len(),
                out.len(),
                100.0 * out.len() as f64 / data.len() as f64
            );
        }
        "texttest" => text_test(
            &args[2],
            args.get(3)
                .map(|s| s.parse().unwrap())
                .unwrap_or(4 * 1024 * 1024),
        ),
        "aliastest" => alias_test(
            &args[2],
            args.get(3).map(|s| s == "1").unwrap_or(false),
            args.get(4).map(|s| s.parse().unwrap_or(0)).unwrap_or(0),
        ),
        "rolztest" => rolz_test(
            &args[2],
            args.get(3).map(|s| s.parse().unwrap_or(0)).unwrap_or(0),
        ),
        "fsdtest" => fsd_test(
            &args[2],
            args.get(3).map(|s| s.parse().unwrap_or(0)).unwrap_or(0),
        ),
        "utftest" => utf_test(
            &args[2],
            args.get(3).map(|s| s.parse().unwrap_or(0)).unwrap_or(0),
        ),
        "exetest" => exe_test(
            &args[2],
            args.get(3).map(|s| s.parse().unwrap_or(0)).unwrap_or(0),
        ),
        "ranktest" => rank_test(&args[2]),
        "srttest" => srt_test(&args[2]),
        "text1test" => text1_test(&args[2]),
        "bwttest" => bwt_test(&args[2]),
        "fpaqtest" => fpaq_test(&args[2], args.get(3).cloned()),
        "fpaqenc" => {
            let data = fs::read(&args[2]).expect("read input");
            let mut enc = fpaq::FpaqEncoder::new();
            let mut bw = BitWriter::new();
            enc.write(&data, &mut bw);
            enc.dispose(&mut bw);
            let (bytes, _) = bw.finish_with_len();
            fs::write(&args[3], &bytes).expect("write output");
            println!("fpaq-encoded {} -> {} bytes", data.len(), bytes.len());
        }
        "zrltfwdtest" => zrlt_test(&args[2]),
        "l4stages" => {
            let data = fs::read(&args[2]).expect("read input");
            let block_size: u32 = args
                .get(3)
                .map(|s| s.parse().unwrap())
                .unwrap_or(4 * 1024 * 1024);
            let mut text_dst = vec![0u8; text_codec::max_encoded_len(data.len())];
            match text_codec::forward(&data, &mut text_dst, block_size, false) {
                Ok((_, n, dt)) => {
                    println!("TEXT: ok n={} dt={:?}", n, dt);
                    text_dst.truncate(n);
                    let mut pack_dst = vec![0u8; alias::max_encoded_len(text_dst.len())];
                    match alias::forward(&text_dst, &mut pack_dst, dt, false) {
                        Ok((_, m, dt2)) => {
                            println!("PACK: ok m={} dt={:?}", m, dt2);
                            pack_dst.truncate(m);
                            let mut rolz = rolz::RolzCodec::new();
                            let mut rolz_dst = vec![0u8; rolz::max_encoded_len(pack_dst.len())];
                            match rolz.forward(&pack_dst, &mut rolz_dst, dt2) {
                                Ok((_, k, _)) => println!("ROLZ: ok k={}", k),
                                Err((e, _)) => println!("ROLZ: declined {}", e),
                            }
                        }
                        Err((e, _)) => println!("PACK: declined {}", e),
                    }
                }
                Err((e, _)) => println!("TEXT: declined {}", e),
            }
        }
        "tpaqtest" => {
            let data = fs::read(&args[2]).expect("read input");
            let extra = args.get(3).map(|s| s == "x").unwrap_or(false);
            let mut bw = BitWriter::new();
            let mut enc = binary_entropy::BinaryEntropyEncoder::new(tpaq::TpaqPredictor::new(
                data.len() as u32,
                data.len() as u32,
                extra,
            ));
            enc.write(&data, &mut bw).expect("tpaq encode");
            enc.dispose(&mut bw);
            let (bytes, _) = bw.finish_with_len();
            println!("tpaq-encoded {} -> {} bytes (extra={})", data.len(), bytes.len(), extra);

            let mut br = bitio::BitReader::new(&bytes);
            let mut dec = binary_entropy::BinaryEntropyDecoder::new(tpaq::TpaqPredictor::new(
                data.len() as u32,
                data.len() as u32,
                extra,
            ));
            let mut back = vec![0u8; data.len()];
            dec.read_block(&mut br, &mut back).expect("tpaq decode");

            if back == data {
                println!("ROUNDTRIP MATCH: {} bytes", data.len());
            } else {
                println!("ROUNDTRIP MISMATCH");
                let n = back.len().min(data.len());
                for i in 0..n {
                    if back[i] != data[i] {
                        println!("  first diff at byte {}: got={} want={}", i, back[i], data[i]);
                        break;
                    }
                }
                std::process::exit(1);
            }
        }
        "rlttest" => {
            let data = fs::read(&args[2]).expect("read input");
            let mut dst = vec![0u8; rlt::max_encoded_len(data.len())];
            match rlt::forward(&data, &mut dst, datatype::DataType::Undefined) {
                Ok((r, w, dt)) => {
                    dst.truncate(w);
                    fs::write(&args[3], &dst).expect("write output");
                    println!("rust rlt forward: {} -> {} bytes (consumed {}) dt={:?}", data.len(), w, r, dt);

                    let mut back = vec![0u8; data.len() + 64];
                    match rlt::inverse(&dst, &mut back) {
                        Ok((_, w2)) => {
                            back.truncate(w2);
                            if back == data {
                                println!("rust rlt self-roundtrip ok");
                            } else {
                                println!("rust rlt self-roundtrip MISMATCH");
                                std::process::exit(1);
                            }
                        }
                        Err(e) => {
                            println!("rust rlt inverse failed: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                Err((e, dt)) => {
                    println!("rust rlt forward declined: {} dt={:?}", e, dt);
                    std::process::exit(1);
                }
            }
        }
        "tpaqenc" => {
            let data = fs::read(&args[2]).expect("read input");
            let extra = args.get(4).map(|s| s == "x").unwrap_or(false);
            let rbsz: u32 = args.get(5).map(|s| s.parse().unwrap()).unwrap_or(data.len() as u32);
            let mut bw = BitWriter::new();
            let mut enc =
                binary_entropy::BinaryEntropyEncoder::new(tpaq::TpaqPredictor::new(rbsz, data.len() as u32, extra));
            enc.write(&data, &mut bw).expect("tpaq encode");
            enc.dispose(&mut bw);
            let (bytes, _) = bw.finish_with_len();
            fs::write(&args[3], &bytes).expect("write output");
            println!("rust tpaq-encoded {} -> {} bytes", data.len(), bytes.len());
        }
        "tpaqdec" => {
            // Cross-check against a real-Go-encoded raw TPAQ/binary-entropy
            // payload: tpaqdec <encoded> <out> <origSize> [x]
            let enc_bytes = fs::read(&args[2]).expect("read encoded input");
            let orig_size: u32 = args[4].parse().expect("origSize");
            let extra = args.get(5).map(|s| s == "x").unwrap_or(false);
            let mut br = bitio::BitReader::new(&enc_bytes);
            let mut dec = binary_entropy::BinaryEntropyDecoder::new(tpaq::TpaqPredictor::new(orig_size, orig_size, extra));
            let mut back = vec![0u8; orig_size as usize];
            dec.read_block(&mut br, &mut back).expect("tpaq decode");
            fs::write(&args[3], &back).expect("write output");
            println!("rust tpaq-decoded {} -> {} bytes", enc_bytes.len(), back.len());
        }
        "encode9" => {
            let block_size: u32 = args
                .get(4)
                .map(|s| s.parse().unwrap())
                .unwrap_or(4 * 1024 * 1024);
            let data = fs::read(&args[2]).expect("read input");
            let out = container::encode_level9(&data, block_size);
            fs::write(&args[3], &out).expect("write output");
            println!(
                "encoded {} -> {} bytes ({:.2}%)",
                data.len(),
                out.len(),
                100.0 * out.len() as f64 / data.len() as f64
            );
        }
        "encode8" => {
            let block_size: u32 = args
                .get(4)
                .map(|s| s.parse().unwrap())
                .unwrap_or(4 * 1024 * 1024);
            let data = fs::read(&args[2]).expect("read input");
            let out = container::encode_level8(&data, block_size);
            fs::write(&args[3], &out).expect("write output");
            println!(
                "encoded {} -> {} bytes ({:.2}%)",
                data.len(),
                out.len(),
                100.0 * out.len() as f64 / data.len() as f64
            );
        }
        "encode7" => {
            let block_size: u32 = args
                .get(4)
                .map(|s| s.parse().unwrap())
                .unwrap_or(4 * 1024 * 1024);
            let data = fs::read(&args[2]).expect("read input");
            let out = container::encode_level7(&data, block_size);
            fs::write(&args[3], &out).expect("write output");
            println!(
                "encoded {} -> {} bytes ({:.2}%)",
                data.len(),
                out.len(),
                100.0 * out.len() as f64 / data.len() as f64
            );
        }
        "encode6" => {
            let block_size: u32 = args
                .get(4)
                .map(|s| s.parse().unwrap())
                .unwrap_or(4 * 1024 * 1024);
            let data = fs::read(&args[2]).expect("read input");
            let out = container::encode_level6(&data, block_size);
            fs::write(&args[3], &out).expect("write output");
            println!(
                "encoded {} -> {} bytes ({:.2}%)",
                data.len(),
                out.len(),
                100.0 * out.len() as f64 / data.len() as f64
            );
        }
        "encode5" => {
            let block_size: u32 = args
                .get(4)
                .map(|s| s.parse().unwrap())
                .unwrap_or(4 * 1024 * 1024);
            let data = fs::read(&args[2]).expect("read input");
            let out = container::encode_level5(&data, block_size);
            fs::write(&args[3], &out).expect("write output");
            println!(
                "encoded {} -> {} bytes ({:.2}%)",
                data.len(),
                out.len(),
                100.0 * out.len() as f64 / data.len() as f64
            );
        }
        "encode4" => {
            let block_size: u32 = args
                .get(4)
                .map(|s| s.parse().unwrap())
                .unwrap_or(4 * 1024 * 1024);
            let data = fs::read(&args[2]).expect("read input");
            let out = container::encode_level4(&data, block_size);
            fs::write(&args[3], &out).expect("write output");
            println!(
                "encoded {} -> {} bytes ({:.2}%)",
                data.len(),
                out.len(),
                100.0 * out.len() as f64 / data.len() as f64
            );
        }
        "anstest" => ans_test(
            &args[2],
            args.get(3).map(|s| s.parse().unwrap_or(0)).unwrap_or(0),
            args.get(4).map(|s| s.parse().ok()).flatten(),
            args.get(5).cloned(),
        ),
        "ansenc" => {
            let data = fs::read(&args[2]).expect("read input");
            let order: u32 = args.get(4).map(|s| s.parse().unwrap_or(0)).unwrap_or(0);
            let chunk: Option<usize> = args.get(5).map(|s| s.parse().ok()).flatten();
            let mut enc = ans::AnsEncoder::new(order, chunk, None).expect("ans encoder");
            let mut bw = BitWriter::new();
            enc.write(&data, &mut bw);
            let (bytes, _) = bw.finish_with_len();
            fs::write(&args[3], &bytes).expect("write output");
            println!(
                "ans-encoded {} -> {} bytes (order {})",
                data.len(),
                bytes.len(),
                order
            );
        }
        "huftest_dec" => {
            let data = fs::read(&args[2]).expect("read input");
            let mut enc = HuffmanEncoder::new();
            let mut bw = BitWriter::new();
            enc.write(&data, &mut bw);
            let bytes = bw.finish();

            let mut br = bitio::BitReader::new(&bytes);
            let mut dec = HuffmanDecoderV6::new();
            let mut back = vec![0u8; data.len()];
            if let Err(e) = dec.decode(&mut br, &mut back) {
                println!("ROUNDTRIP DECODE ERROR: {}", e);
                std::process::exit(1);
            }

            if back == data {
                println!("ROUNDTRIP MATCH: {} bytes", data.len());
            } else {
                println!("ROUNDTRIP MISMATCH: {} bytes", data.len());
                let n = back.len().min(data.len());
                for i in 0..n {
                    if back[i] != data[i] {
                        println!(
                            "  first diff at byte {}: got={} want={}",
                            i, back[i], data[i]
                        );
                        break;
                    }
                }
                std::process::exit(1);
            }
        }
        "decode" => {
            let data = fs::read(&args[2]).expect("read input");
            match container::decode(&data) {
                Ok(out) => {
                    fs::write(&args[3], &out).expect("write output");
                    println!("decoded {} -> {} bytes", data.len(), out.len());
                }
                Err(e) => {
                    eprintln!("decode failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        other => {
            eprintln!("unknown subcommand: {}", other);
            std::process::exit(1);
        }
    }
}

fn text_test(prefix: &str, block_size: u32) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");

    let mut dst = vec![0u8; text_codec::max_encoded_len(orig.len())];
    match text_codec::forward(&orig, &mut dst, block_size, false) {
        Ok((src_len, dst_len, dt)) => {
            dst.truncate(dst_len);
            println!(
                "rust forward ok: src_len={} dst_len={} dt={:?}",
                src_len, dst_len, dt
            );

            if dst == go_fwd {
                println!(
                    "FORWARD MATCH: {} bytes (ratio {:.2}%)",
                    dst_len,
                    100.0 * dst_len as f64 / orig.len() as f64
                );
            } else {
                println!(
                    "FORWARD MISMATCH: rust={} bytes go={} bytes",
                    dst.len(),
                    go_fwd.len()
                );
                let n = dst.len().min(go_fwd.len());
                for i in 0..n {
                    if dst[i] != go_fwd[i] {
                        println!(
                            "  first diff at byte {}: rust={} go={}",
                            i, dst[i], go_fwd[i]
                        );
                        break;
                    }
                }
                std::process::exit(1);
            }

            let mut back = vec![0u8; orig.len() + 16];
            match text_codec::inverse(&go_fwd, &mut back, block_size, false) {
                Ok((_, back_len)) => {
                    back.truncate(back_len);

                    if back == orig {
                        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
                    } else {
                        println!(
                            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
                            back_len,
                            orig.len()
                        );
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    println!("rust inverse failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err((e, dt)) => {
            println!("rust forward declined: {} (dt={:?})", e, dt);

            if !go_fwd.is_empty() {
                println!("MISMATCH: go did not decline but rust did");
                std::process::exit(1);
            } else {
                println!("MATCH (both decline)");
            }
        }
    }
}

fn ans_test(input: &str, order: u32, chunk: Option<usize>, goenc: Option<String>) {
    let data = fs::read(input).expect("read input");

    // Self roundtrip through our own encoder/decoder.
    let mut enc = ans::AnsEncoder::new(order, chunk, None).expect("ans encoder");
    let mut bw = BitWriter::new();
    enc.write(&data, &mut bw);
    let (bytes, _) = bw.finish_with_len();

    let mut br = bitio::BitReader::new(&bytes);
    let mut dec = ans::AnsDecoder::new(order, chunk).expect("ans decoder");
    let mut back = vec![0u8; data.len()];

    match dec.read(&mut br, &mut back) {
        Ok(n) => {
            if n == data.len() && back == data {
                println!(
                    "ANS ROUNDTRIP MATCH: {} bytes -> {} bytes (order {:?})",
                    data.len(),
                    bytes.len(),
                    chunk
                );
            } else {
                println!("ANS ROUNDTRIP MISMATCH: got {} want {}", n, data.len());
                std::process::exit(1);
            }
        }
        Err(e) => {
            println!("ANS ROUNDTRIP DECODE ERROR: {}", e);
            std::process::exit(1);
        }
    }

    // Cross-check: decode Go-encoder output (same order/chunk) with our decoder.
    if let Some(g) = goenc {
        let go_bytes = fs::read(&g).expect("read goenc");
        let mut br = bitio::BitReader::new(&go_bytes);
        let mut dec = ans::AnsDecoder::new(order, chunk).expect("ans decoder");
        let mut back = vec![0u8; data.len()];

        match dec.read(&mut br, &mut back) {
            Ok(n) => {
                if n == data.len() && back == data {
                    println!("ANS GO-DECODE MATCH: {} bytes", n);
                } else {
                    println!("ANS GO-DECODE MISMATCH: got {} want {}", n, data.len());
                    std::process::exit(1);
                }
            }
            Err(e) => {
                println!("ANS GO-DECODE ERROR: {}", e);
                std::process::exit(1);
            }
        }
    }
}

fn fsd_test(prefix: &str, dt_code: u8) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");
    let dt_in = dt_from_code(dt_code);

    let mut dst = vec![0u8; fsd::max_encoded_len(orig.len())];
    match fsd::forward(&orig, &mut dst, dt_in) {
        Ok((src_len, dst_len, dt)) => {
            dst.truncate(dst_len);
            println!(
                "rust forward ok: src_len={} dst_len={} dt={:?}",
                src_len, dst_len, dt
            );

            if dst == go_fwd {
                println!(
                    "FORWARD MATCH: {} bytes (ratio {:.2}%)",
                    dst_len,
                    100.0 * dst_len as f64 / orig.len() as f64
                );
            } else {
                println!(
                    "FORWARD MISMATCH: rust={} bytes go={} bytes",
                    dst.len(),
                    go_fwd.len()
                );
                let n = dst.len().min(go_fwd.len());
                for i in 0..n {
                    if dst[i] != go_fwd[i] {
                        println!(
                            "  first diff at byte {}: rust={} go={}",
                            i, dst[i], go_fwd[i]
                        );
                        break;
                    }
                }
                std::process::exit(1);
            }

            let mut back = vec![0u8; orig.len() + 1024];
            match fsd::inverse(&go_fwd, &mut back) {
                Ok((_, back_len)) => {
                    back.truncate(back_len);

                    if back == orig {
                        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
                    } else {
                        println!(
                            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
                            back_len,
                            orig.len()
                        );
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    println!("rust inverse failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err((e, dt)) => {
            println!("rust forward declined: {} (dt={:?})", e, dt);

            if !go_fwd.is_empty() {
                println!("MISMATCH: go did not decline but rust did");
                std::process::exit(1);
            } else {
                println!("MATCH (both decline)");
            }
        }
    }
}

fn fpaq_test(input: &str, goenc: Option<String>) {
    let data = fs::read(input).expect("read input");

    // Self roundtrip through our own encoder/decoder.
    let mut enc = fpaq::FpaqEncoder::new();
    let mut bw = BitWriter::new();
    enc.write(&data, &mut bw);
    enc.dispose(&mut bw);
    let (bytes, _) = bw.finish_with_len();

    let mut br = bitio::BitReader::new(&bytes);
    let mut dec = fpaq::FpaqDecoder::new();
    let mut back = vec![0u8; data.len()];

    match dec.read_block(&mut br, &mut back) {
        Ok(n) => {
            if n == data.len() && back == data {
                println!(
                    "FPAQ ROUNDTRIP MATCH: {} bytes -> {} bytes",
                    data.len(),
                    bytes.len()
                );
            } else {
                println!("FPAQ ROUNDTRIP MISMATCH: got {} want {}", n, data.len());
                std::process::exit(1);
            }
        }
        Err(e) => {
            println!("FPAQ ROUNDTRIP DECODE ERROR: {}", e);
            std::process::exit(1);
        }
    }

    // Cross-check: decode Go-encoder output with our decoder.
    if let Some(g) = goenc {
        let go_bytes = fs::read(&g).expect("read goenc");
        let mut br = bitio::BitReader::new(&go_bytes);
        let mut dec = fpaq::FpaqDecoder::new();
        let mut back = vec![0u8; data.len()];

        match dec.read_block(&mut br, &mut back) {
            Ok(n) => {
                if n == data.len() && back == data {
                    println!("FPAQ GO-DECODE MATCH: {} bytes", n);
                } else {
                    println!("FPAQ GO-DECODE MISMATCH: got {} want {}", n, data.len());
                    std::process::exit(1);
                }
            }
            Err(e) => {
                println!("FPAQ GO-DECODE ERROR: {}", e);
                std::process::exit(1);
            }
        }
    }
}

fn bwt_test(prefix: &str) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");

    let mut codec = bwt::Bwt::new();
    let mut dst = vec![0u8; bwt::max_encoded_len(orig.len())];
    match codec.forward(&orig, &mut dst) {
        Ok((src_len, dst_len)) => {
            dst.truncate(dst_len);
            println!("rust forward ok: src_len={} dst_len={}", src_len, dst_len);

            if dst == go_fwd {
                println!(
                    "FORWARD MATCH: {} bytes (ratio {:.2}%)",
                    dst_len,
                    100.0 * dst_len as f64 / orig.len() as f64
                );
            } else {
                println!(
                    "FORWARD MISMATCH: rust={} bytes go={} bytes",
                    dst.len(),
                    go_fwd.len()
                );
                let n = dst.len().min(go_fwd.len());
                for i in 0..n {
                    if dst[i] != go_fwd[i] {
                        println!(
                            "  first diff at byte {}: rust={} go={}",
                            i, dst[i], go_fwd[i]
                        );
                        break;
                    }
                }
                std::process::exit(1);
            }

            let mut back = vec![0u8; orig.len() + 64];
            match codec.inverse(&go_fwd, &mut back) {
                Ok((_, back_len)) => {
                    back.truncate(back_len);

                    if back == orig {
                        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
                    } else {
                        println!(
                            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
                            back_len,
                            orig.len()
                        );
                        let n = back.len().min(orig.len());
                        for i in 0..n {
                            if back[i] != orig[i] {
                                println!(
                                    "  first diff at byte {}: got={} want={}",
                                    i, back[i], orig[i]
                                );
                                break;
                            }
                        }
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    println!("rust inverse failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            println!("rust forward declined: {}", e);

            if !go_fwd.is_empty() {
                println!("MISMATCH: go did not decline but rust did");
                std::process::exit(1);
            } else {
                println!("MATCH (both decline)");
            }
        }
    }
}

fn text1_test(prefix: &str) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");
    let block_size = orig.len() as u32;

    // textCodec1 needs the same srcLen-sized output buffer as Go's delegate.
    let mut dst = vec![0u8; text_codec::max_encoded_len(orig.len())];
    match text_codec1::forward(&orig, &mut dst, block_size, false) {
        Ok((src_len, dst_len, dt)) => {
            dst.truncate(dst_len);
            println!("rust forward ok: src_len={} dst_len={} dt={:?}", src_len, dst_len, dt);

            if dst == go_fwd {
                println!("FORWARD MATCH: {} bytes", dst_len);
            } else {
                println!(
                    "FORWARD MISMATCH: rust={} bytes go={} bytes",
                    dst.len(),
                    go_fwd.len()
                );
                let n = dst.len().min(go_fwd.len());
                for i in 0..n {
                    if dst[i] != go_fwd[i] {
                        println!("  first diff at byte {}: rust={} go={}", i, dst[i], go_fwd[i]);
                        break;
                    }
                }
                std::process::exit(1);
            }

            let mut back = vec![0u8; orig.len() + 64];
            match text_codec1::inverse(&go_fwd, &mut back, block_size, false) {
                Ok((_, back_len)) => {
                    back.truncate(back_len);

                    if back == orig {
                        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
                    } else {
                        println!(
                            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
                            back_len,
                            orig.len()
                        );
                        let n = back.len().min(orig.len());
                        for i in 0..n {
                            if back[i] != orig[i] {
                                println!("  first diff at byte {}: got={} want={}", i, back[i], orig[i]);
                                break;
                            }
                        }
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    println!("rust inverse failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err((e, dt)) => {
            println!("rust forward declined: {} (dt={:?})", e, dt);

            if !go_fwd.is_empty() {
                println!("MISMATCH: go did not decline but rust did");
                std::process::exit(1);
            } else {
                println!("MATCH (both decline)");
            }
        }
    }
}

fn srt_test(prefix: &str) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");

    let codec = srt::Srt::new();
    let mut dst = vec![0u8; srt::Srt::max_encoded_len(orig.len())];
    match codec.forward(&orig, &mut dst) {
        Ok((src_len, dst_len)) => {
            dst.truncate(dst_len);
            println!("rust forward ok: src_len={} dst_len={}", src_len, dst_len);

            if dst == go_fwd {
                println!("FORWARD MATCH: {} bytes", dst_len);
            } else {
                println!(
                    "FORWARD MISMATCH: rust={} bytes go={} bytes",
                    dst.len(),
                    go_fwd.len()
                );
                let n = dst.len().min(go_fwd.len());
                for i in 0..n {
                    if dst[i] != go_fwd[i] {
                        println!(
                            "  first diff at byte {}: rust={} go={}",
                            i, dst[i], go_fwd[i]
                        );
                        break;
                    }
                }
                std::process::exit(1);
            }

            let mut back = vec![0u8; orig.len() + 64];
            match codec.inverse(&go_fwd, &mut back) {
                Ok((_, back_len)) => {
                    back.truncate(back_len);

                    if back == orig {
                        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
                    } else {
                        println!(
                            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
                            back_len,
                            orig.len()
                        );
                        let n = back.len().min(orig.len());
                        for i in 0..n {
                            if back[i] != orig[i] {
                                println!(
                                    "  first diff at byte {}: got={} want={}",
                                    i, back[i], orig[i]
                                );
                                break;
                            }
                        }
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    println!("rust inverse failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            println!("rust forward declined: {}", e);

            if !go_fwd.is_empty() {
                println!("MISMATCH: go did not decline but rust did");
                std::process::exit(1);
            } else {
                println!("MATCH (both decline)");
            }
        }
    }
}

fn rank_test(prefix: &str) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");

    let codec = sbrt::Sbrt::new_rank();
    let mut dst = vec![0u8; sbrt::Sbrt::max_encoded_len(orig.len())];
    match codec.forward(&orig, &mut dst) {
        Ok((src_len, dst_len)) => {
            dst.truncate(dst_len);
            println!("rust forward ok: src_len={} dst_len={}", src_len, dst_len);

            if dst == go_fwd {
                println!("FORWARD MATCH: {} bytes", dst_len);
            } else {
                println!(
                    "FORWARD MISMATCH: rust={} bytes go={} bytes",
                    dst.len(),
                    go_fwd.len()
                );
                let n = dst.len().min(go_fwd.len());
                for i in 0..n {
                    if dst[i] != go_fwd[i] {
                        println!(
                            "  first diff at byte {}: rust={} go={}",
                            i, dst[i], go_fwd[i]
                        );
                        break;
                    }
                }
                std::process::exit(1);
            }

            let mut back = vec![0u8; orig.len() + 64];
            match codec.inverse(&go_fwd, &mut back) {
                Ok((_, back_len)) => {
                    back.truncate(back_len);

                    if back == orig {
                        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
                    } else {
                        println!(
                            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
                            back_len,
                            orig.len()
                        );
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    println!("rust inverse failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            println!("rust forward declined: {}", e);

            if !go_fwd.is_empty() {
                println!("MISMATCH: go did not decline but rust did");
                std::process::exit(1);
            } else {
                println!("MATCH (both decline)");
            }
        }
    }
}

fn zrlt_test(prefix: &str) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");

    let mut dst = vec![0u8; zrlt::max_encoded_len(orig.len())];
    match zrlt::forward(&orig, &mut dst) {
        Ok((src_len, dst_len)) => {
            dst.truncate(dst_len);
            println!("rust forward ok: src_len={} dst_len={}", src_len, dst_len);

            if dst == go_fwd {
                println!("FORWARD MATCH: {} bytes", dst_len);
            } else {
                println!(
                    "FORWARD MISMATCH: rust={} bytes go={} bytes",
                    dst.len(),
                    go_fwd.len()
                );
                let n = dst.len().min(go_fwd.len());
                for i in 0..n {
                    if dst[i] != go_fwd[i] {
                        println!(
                            "  first diff at byte {}: rust={} go={}",
                            i, dst[i], go_fwd[i]
                        );
                        break;
                    }
                }
                std::process::exit(1);
            }

            let mut back = vec![0u8; orig.len() + 64];
            match zrlt::inverse(&go_fwd, &mut back) {
                Ok((_, back_len)) => {
                    back.truncate(back_len);

                    if back == orig {
                        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
                    } else {
                        println!(
                            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
                            back_len,
                            orig.len()
                        );
                        let n = back.len().min(orig.len());
                        for i in 0..n {
                            if back[i] != orig[i] {
                                println!(
                                    "  first diff at byte {}: got={} want={}",
                                    i, back[i], orig[i]
                                );
                                break;
                            }
                        }
                        fs::write(format!("{}.rsout", prefix), &back).ok();
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    println!("rust inverse failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            println!("rust forward declined: {}", e);

            if !go_fwd.is_empty() {
                println!("MISMATCH: go did not decline but rust did");
                std::process::exit(1);
            } else {
                println!("MATCH (both decline)");
            }
        }
    }
}

fn exe_test(prefix: &str, dt_code: u8) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");
    let dt_in = dt_from_code(dt_code);

    let mut dst = vec![0u8; exe::max_encoded_len(orig.len())];
    match exe::forward(&orig, &mut dst, dt_in) {
        Ok((src_len, dst_len, dt)) => {
            dst.truncate(dst_len);
            println!(
                "rust forward ok: src_len={} dst_len={} dt={:?}",
                src_len, dst_len, dt
            );

            if dst == go_fwd {
                println!(
                    "FORWARD MATCH: {} bytes (ratio {:.2}%)",
                    dst_len,
                    100.0 * dst_len as f64 / orig.len() as f64
                );
            } else {
                println!(
                    "FORWARD MISMATCH: rust={} bytes go={} bytes",
                    dst.len(),
                    go_fwd.len()
                );
                let n = dst.len().min(go_fwd.len());
                for i in 0..n {
                    if dst[i] != go_fwd[i] {
                        println!(
                            "  first diff at byte {}: rust={} go={}",
                            i, dst[i], go_fwd[i]
                        );
                        break;
                    }
                }
                std::process::exit(1);
            }

            let mut back = vec![0u8; orig.len() + 1024];
            match exe::inverse(&go_fwd, &mut back) {
                Ok((_, back_len)) => {
                    back.truncate(back_len);

                    if back == orig {
                        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
                    } else {
                        println!(
                            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
                            back_len,
                            orig.len()
                        );
                        let n = back.len().min(orig.len());
                        for i in 0..n {
                            if back[i] != orig[i] {
                                println!(
                                    "  first diff at byte {}: got={} want={}",
                                    i, back[i], orig[i]
                                );
                                break;
                            }
                        }
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    println!("rust inverse failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err((e, dt)) => {
            println!("rust forward declined: {} (dt={:?})", e, dt);

            if !go_fwd.is_empty() {
                println!("MISMATCH: go did not decline but rust did");
                std::process::exit(1);
            } else {
                println!("MATCH (both decline)");
            }
        }
    }
}

fn utf_test(prefix: &str, dt_code: u8) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");
    let dt_in = dt_from_code(dt_code);

    let mut dst = vec![0u8; utf::max_encoded_len(orig.len())];
    match utf::forward(&orig, &mut dst, dt_in) {
        Ok((src_len, dst_len, dt)) => {
            dst.truncate(dst_len);
            println!(
                "rust forward ok: src_len={} dst_len={} dt={:?}",
                src_len, dst_len, dt
            );

            if dst == go_fwd {
                println!(
                    "FORWARD MATCH: {} bytes (ratio {:.2}%)",
                    dst_len,
                    100.0 * dst_len as f64 / orig.len() as f64
                );
            } else {
                println!(
                    "FORWARD MISMATCH: rust={} bytes go={} bytes",
                    dst.len(),
                    go_fwd.len()
                );
                let n = dst.len().min(go_fwd.len());
                for i in 0..n {
                    if dst[i] != go_fwd[i] {
                        println!(
                            "  first diff at byte {}: rust={} go={}",
                            i, dst[i], go_fwd[i]
                        );
                        break;
                    }
                }
                std::process::exit(1);
            }

            let mut back = vec![0u8; orig.len() + 1024];
            match utf::inverse(&go_fwd, &mut back) {
                Ok((_, back_len)) => {
                    back.truncate(back_len);

                    if back == orig {
                        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
                    } else {
                        println!(
                            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
                            back_len,
                            orig.len()
                        );
                        let n = back.len().min(orig.len());
                        for i in 0..n {
                            if back[i] != orig[i] {
                                println!(
                                    "  first diff at byte {}: got={} want={}",
                                    i, back[i], orig[i]
                                );
                                break;
                            }
                        }
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    println!("rust inverse failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err((e, dt)) => {
            println!("rust forward declined: {} (dt={:?})", e, dt);

            if !go_fwd.is_empty() {
                println!("MISMATCH: go did not decline but rust did");
                std::process::exit(1);
            } else {
                println!("MATCH (both decline)");
            }
        }
    }
}

fn rolz_test(prefix: &str, dt_code: u8) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");

    let mut codec = rolz::RolzCodec::new();
    let mut dst = vec![0u8; rolz::max_encoded_len(orig.len())];
    match codec.forward(&orig, &mut dst, dt_from_code(dt_code)) {
        Ok((src_len, dst_len, dt)) => {
            dst.truncate(dst_len);
            println!(
                "rust forward ok: src_len={} dst_len={} dt={:?}",
                src_len, dst_len, dt
            );

            if dst == go_fwd {
                println!(
                    "FORWARD MATCH: {} bytes (ratio {:.2}%)",
                    dst_len,
                    100.0 * dst_len as f64 / orig.len() as f64
                );
            } else {
                println!(
                    "FORWARD MISMATCH: rust={} bytes go={} bytes",
                    dst.len(),
                    go_fwd.len()
                );
                let n = dst.len().min(go_fwd.len());
                for i in 0..n {
                    if dst[i] != go_fwd[i] {
                        println!(
                            "  first diff at byte {}: rust={} go={}",
                            i, dst[i], go_fwd[i]
                        );
                        break;
                    }
                }
                std::process::exit(1);
            }

            let mut back = vec![0u8; orig.len() + 64];
            match codec.inverse(&go_fwd, &mut back) {
                Ok((_, back_len)) => {
                    back.truncate(back_len);

                    if back == orig {
                        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
                    } else {
                        println!(
                            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
                            back_len,
                            orig.len()
                        );
                        let n = back.len().min(orig.len());
                        for i in 0..n {
                            if back[i] != orig[i] {
                                println!(
                                    "  first diff at byte {}: got={} want={}",
                                    i, back[i], orig[i]
                                );
                                break;
                            }
                        }
                        fs::write(format!("{}.rsout", prefix), &back).ok();
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    println!("rust inverse failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err((e, dt)) => {
            println!("rust forward declined: {} (dt={:?})", e, dt);

            if !go_fwd.is_empty() {
                println!("MISMATCH: go did not decline but rust did");
                std::process::exit(1);
            } else {
                println!("MATCH (both decline)");
            }
        }
    }
}

fn dt_from_code(code: u8) -> datatype::DataType {
    match code {
        1 => datatype::DataType::Text,
        2 => datatype::DataType::Multimedia,
        3 => datatype::DataType::Exe,
        4 => datatype::DataType::Numeric,
        5 => datatype::DataType::Base64,
        6 => datatype::DataType::Dna,
        7 => datatype::DataType::Bin,
        8 => datatype::DataType::Utf8,
        9 => datatype::DataType::SmallAlphabet,
        _ => datatype::DataType::Undefined,
    }
}

fn alias_test(prefix: &str, only_dna: bool, dt_code: u8) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");
    let dt_in = dt_from_code(dt_code);

    let mut dst = vec![0u8; alias::max_encoded_len(orig.len())];
    match alias::forward(&orig, &mut dst, dt_in, only_dna) {
        Ok((src_len, dst_len, dt)) => {
            dst.truncate(dst_len);
            println!(
                "rust forward ok: src_len={} dst_len={} dt={:?}",
                src_len, dst_len, dt
            );

            if dst == go_fwd {
                println!(
                    "FORWARD MATCH: {} bytes (ratio {:.2}%)",
                    dst_len,
                    100.0 * dst_len as f64 / orig.len() as f64
                );
            } else {
                println!(
                    "FORWARD MISMATCH: rust={} bytes go={} bytes",
                    dst.len(),
                    go_fwd.len()
                );
                let n = dst.len().min(go_fwd.len());
                for i in 0..n {
                    if dst[i] != go_fwd[i] {
                        println!(
                            "  first diff at byte {}: rust={} go={}",
                            i, dst[i], go_fwd[i]
                        );
                        break;
                    }
                }
                std::process::exit(1);
            }

            let mut back = vec![0u8; orig.len() + 1024];
            match alias::inverse(&go_fwd, &mut back) {
                Ok((_, back_len)) => {
                    back.truncate(back_len);

                    if back == orig {
                        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
                    } else {
                        println!(
                            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
                            back_len,
                            orig.len()
                        );
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    println!("rust inverse failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err((e, dt)) => {
            println!("rust forward declined: {} (dt={:?})", e, dt);

            if !go_fwd.is_empty() {
                println!("MISMATCH: go did not decline but rust did");
                std::process::exit(1);
            } else {
                println!("MATCH (both decline)");
            }
        }
    }
}

fn lzx_test(prefix: &str) {
    let orig = fs::read(format!("{}.orig", prefix)).expect("read .orig");
    let go_fwd = fs::read(format!("{}.fwd", prefix)).expect("read .fwd");

    let mut codec = LzxCodec::new(true);
    let mut dst = vec![0u8; LzxCodec::max_encoded_len(orig.len())];
    let (src_len, dst_len) = codec
        .forward(&orig, &mut dst, lzx::MIN_MATCH4)
        .expect("rust forward failed");
    dst.truncate(dst_len);

    assert_eq!(src_len, orig.len(), "src_len mismatch");

    if dst == go_fwd {
        println!(
            "FORWARD MATCH: {} bytes (ratio {:.2}%)",
            dst_len,
            100.0 * dst_len as f64 / orig.len() as f64
        );
    } else {
        println!(
            "FORWARD MISMATCH: rust={} bytes go={} bytes",
            dst.len(),
            go_fwd.len()
        );
        let n = dst.len().min(go_fwd.len());
        for i in 0..n {
            if dst[i] != go_fwd[i] {
                println!(
                    "  first diff at byte {}: rust={} go={}",
                    i, dst[i], go_fwd[i]
                );
                break;
            }
        }
        std::process::exit(1);
    }

    let mut guarded = go_fwd.clone();
    guarded.extend_from_slice(&[0u8; lzx::READ_LENGTH_GUARD]);
    let mut back = vec![0u8; orig.len() + 16];
    let (_, back_len) =
        LzxCodec::inverse(&guarded[..go_fwd.len()], &mut back).expect("rust inverse failed");
    back.truncate(back_len);

    if back == orig {
        println!("INVERSE(go_fwd) MATCH: recovered {} bytes", back_len);
    } else {
        println!(
            "INVERSE(go_fwd) MISMATCH: got {} bytes, want {}",
            back_len,
            orig.len()
        );
        std::process::exit(1);
    }
}
