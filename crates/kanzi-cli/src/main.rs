//! `rust_kanzi`: encode/decode .knz files with the `kanzi` crate, plus the
//! cross-check subcommands used to verify bitstream parity against kanzi-go
//! and kanzi-cpp fixtures.

use kanzi::bitio::BitWriter;
use kanzi::huffman_dec::HuffmanDecoderV6;
use kanzi::huffman_enc::HuffmanEncoder;
use kanzi::lzx::LzxCodec;
use kanzi::{
    alias, ans, binary_entropy, bitio, bwt, container, datatype, default_block_size, exe, fpaq, fsd, lzx, rlt,
    rolz, sais, sbrt, srt, text_codec, text_codec1, tpaq, utf, xxhash, zrlt,
};
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
        "encode1" => encode_file(1, &args),
        "encode2" => encode_file(2, &args),
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
        "encode3" => encode_file(3, &args),
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
        "xxhashtest" => {
            let data = fs::read(&args[2]).expect("read input");
            let seed32: u32 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(0);
            let seed64: u64 = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(0);
            let h32 = xxhash::XxHash32::new(seed32).hash(&data);
            let h64 = xxhash::XxHash64::new(seed64).hash(&data);
            println!("xxh32={:#010x} xxh64={:#018x}", h32, h64);
        }
        "saistest" => {
            // Brute-force cross-check of sais::suffix_array against a naive
            // O(n^2 log n) reference sort, on small/pathological inputs
            // (tiny alphabets, long runs, all-same-byte) where SA-IS's
            // recursion and edge cases are most likely to misbehave.
            fn suffix_less(text: &[u8], a: usize, b: usize) -> std::cmp::Ordering {
                let n = text.len();
                let mut i = a;
                let mut j = b;
                loop {
                    let ci = if i < n { Some(text[i]) } else { None };
                    let cj = if j < n { Some(text[j]) } else { None };
                    match (ci, cj) {
                        (None, None) => return std::cmp::Ordering::Equal,
                        (None, Some(_)) => return std::cmp::Ordering::Less,
                        (Some(_), None) => return std::cmp::Ordering::Greater,
                        (Some(x), Some(y)) if x != y => return x.cmp(&y),
                        _ => {
                            i += 1;
                            j += 1;
                        }
                    }
                }
            }

            fn reference_sa(text: &[u8]) -> Vec<u32> {
                let mut idx: Vec<u32> = (0..text.len() as u32).collect();
                idx.sort_by(|&a, &b| suffix_less(text, a as usize, b as usize));
                idx
            }

            fn check(text: &[u8], label: &str) -> bool {
                let got = sais::suffix_array(text);
                let want = reference_sa(text);
                if got == want {
                    true
                } else {
                    println!("[{}] MISMATCH len={}", label, text.len());
                    println!("  got:  {:?}", &got[..got.len().min(40)]);
                    println!("  want: {:?}", &want[..want.len().min(40)]);
                    false
                }
            }

            let mut all_ok = true;
            let mut seed: u64 = 0x1234_5678_9abc_def0;
            let mut rng = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };

            // Fixed pathological cases.
            let cases: Vec<(Vec<u8>, &str)> = vec![
                (b"aa".to_vec(), "aa"),
                (b"ab".to_vec(), "ab"),
                (b"aaa".to_vec(), "aaa"),
                (b"aaaa".to_vec(), "aaaa"),
                (vec![0u8; 200], "all-zero-200"),
                (vec![255u8; 200], "all-0xFF-200"),
                (b"banana".to_vec(), "banana"),
                (b"mississippi".to_vec(), "mississippi"),
                (b"abababababababab".to_vec(), "abab-16"),
                ((0..=255u16).map(|x| x as u8).collect(), "0..255"),
                ((0..=255u16).rev().map(|x| x as u8).collect(), "255..0"),
            ];

            for (text, label) in &cases {
                if !check(text, label) {
                    all_ok = false;
                }
            }

            // Random small/medium inputs, varying alphabet size (small
            // alphabets stress SA-IS's recursion the most).
            for trial in 0..500 {
                let len = 1 + (rng() % 300) as usize;
                let alphabet: u32 = [1, 2, 4, 8, 26, 256][(trial % 6) as usize];
                let text: Vec<u8> = (0..len).map(|_| (rng() % alphabet as u64) as u8).collect();
                if !check(&text, &format!("random#{trial} len={len} alphabet={alphabet}")) {
                    all_ok = false;
                }
            }

            if all_ok {
                println!("saistest: ALL OK");
            } else {
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
        "encode9" => encode_file(9, &args),
        "encode8" => encode_file(8, &args),
        "encode0" => encode_file(0, &args),
        "encode7" => encode_file(7, &args),
        "encode6" => encode_file(6, &args),
        "encode5" => encode_file(5, &args),
        "encode4" => encode_file(4, &args),
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
        // decode <input.knz> <output> [reps]
        //
        // Streams file to file: memory is bounded by the block size, not the
        // file size. The reported time covers reading, decoding and writing,
        // like kanzi-cpp's own "Decompression time" (which likewise excludes
        // process start-up); with `reps` > 1 the fastest run is reported.
        // Thread count follows KANZI_JOBS, the equivalent of kanzi-cpp's -j.
        "decode" => {
            let reps: usize = args.get(4).map(|s| s.parse().unwrap_or(1)).unwrap_or(1);
            let mut best = f64::MAX;
            let mut sizes = (0, 0);

            for _ in 0..reps.max(1) {
                let t0 = std::time::Instant::now();
                let input = fs::File::open(&args[2]).expect("open input");
                let mut output = fs::File::create(&args[3]).expect("create output");

                let written = match kanzi::decompress_to(&input, &mut output) {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("decode failed: {}", e);
                        std::process::exit(1);
                    }
                };

                drop(output);
                best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
                sizes = (input.metadata().map(|m| m.len()).unwrap_or(0), written);
            }

            println!(
                "decoded {} -> {} bytes in {:.1} ms (best of {})",
                sizes.0,
                sizes.1,
                best,
                reps.max(1)
            );
        }
        other => {
            eprintln!("unknown subcommand: {}", other);
            std::process::exit(1);
        }
    }
}

/// `encodeN <input> <output> [blockSize] [ckSize]`: streams file to file, so
/// memory is bounded by the block size rather than the file size.
fn encode_file(level: u32, args: &[String]) {
    let block_size: u32 = args
        .get(4)
        .map(|s| s.parse().unwrap())
        .unwrap_or_else(|| default_block_size(level));
    let ck_size: u64 = args.get(5).map(|s| s.parse().unwrap()).unwrap_or(0);
    let input = fs::File::open(&args[2]).expect("open input");
    let in_len = input.metadata().map(|m| m.len()).unwrap_or(0);
    let mut output = fs::File::create(&args[3]).expect("create output");

    match container::encode_to(input, &mut output, level, block_size, ck_size) {
        Ok(written) => println!(
            "encoded {} -> {} bytes ({:.2}%)",
            in_len,
            written,
            100.0 * written as f64 / in_len.max(1) as f64
        ),
        Err(e) => {
            eprintln!("encode failed: {}", e);
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
