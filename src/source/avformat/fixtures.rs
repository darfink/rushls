//! Encoded fixtures shared by AVFormat and CMAF integration tests.

use base64::{Engine as _, engine::general_purpose::STANDARD};

/// One 48 kHz mono AAC frame in a roughly 4 KiB Matroska fixture.
///
/// Generated once with:
/// `ffmpeg -f lavfi -i sine=frequency=1000:sample_rate=48000:duration=0.0213333333 -c:a aac -b:a 96k -f matroska primed-aac.mkv`
///
/// Text encoding keeps the small binary fixture reviewable while tests remain
/// independent of an installed FFmpeg CLI or encoder.
const PRIMED_AAC_MKV_BASE64: &str = "GkXfo6NChoEBQveBAULygQRC84EIQoKIbWF0cm9za2FCh4EEQoWBAhhTgGcBAAAAAAAESBFNm3TAv4TpTQJRTbuLU6uEFUmpZlOsgaFNu4tTq4QWVK5rU6yB8U27jFOrhBJUw2dTrIIBUE27jFOrhBxTu2tTrIIELOwBAAAAAAAAUwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAFUmpZsu/hGgBhhQq17GDD0JATYCNTGF2ZjYyLjEyLjEwMldBjUxhdmY2Mi4xMi4xMDJzpJAUI7mZvCLwr4VgUdLoTinyRImIQEUAAAAAAAAWVK5r2r+EVJRXz64BAAAAAAAAS9eBAXPFiPnt8UOt1U0snIEAIrWcg3VuZIiBAIaFQV9BQUNWqoQBRYVVg4EC4ZGfgQG1iEDncAAAAAAAYmSBIFXugQBjooURiFblABJUw2dAf7+EnldwunNzoGPAgGfImkWjh0VOQ09ERVJEh41MYXZmNjIuMTIuMTAyc3PTY8CLY8WI+e3xQ63VTSxnyJ5Fo4dFTkNPREVSRIeRTGF2YzYyLjI4LjEwMiBhYWNnyKFFo4hEVVJBVElPTkSHkzAwOjAwOjAwLjA0MjAwMDAwMAAfQ7Z1QlG/hLeUcqPngQCjQRqBAACA3gIATGF2YzYyLjI4LjEwMgACUG1c6ZokHqXfXHeq5yvX760uuJUk5mufPMj0nPnvQggtq2Wpptq2fXvqNtWz2V2r2V2rbWYbazDpLznRRAhNUw67Q/au8dnfM95c07O+Z7y5p4u2bo7NOLbN2do3NWE01TNNc27O2bs7NOatm62zTmrNOattyrKcdjcdlOOxuOxuOxuOxuOxuOxuOxuOxuOxuOxtiUqlKpSqUqlKpSqUqlKpSqUqn1+fX59fn1+fX59VElElElElElElPr8+vz6/Pr8+vz6/PqokokokokokokokokokokokokokokokokokokokokokokpwVLLLLLLLL/l/y/5f8v+X/L/l/yyyyyyyyy+jQSiBABWAARo2rZVGJqX7L5zzWbr4rSbk1OJV1fPG98b789waBAbG2aeOnjqsqr1y9c+3w+40f8aP+qOJEQotQEh9acT1Rju4XOsVvXMfjLnWK3WK3WK3WK2oVqFahWoVqFahWoVqFcZHxkfGR8ZHxkfGR+UZXlGV5RleUZXtm35RleUZXlGV2Cx2Cx2Cx2Cx2Cx5RleUZXlGV5RleUZXYLHYLHYLHYLHYLHYLHlGV5RleUZXlGV5RleUZW0btG7Ru0btG7Ru0b4jF4jF4ix2Cx2Cx2Cx2CxtG7Ru0btG7Ru0btG7RuYNmDZg2YNmDZg2YNmDbRu0btG7Ru0btG7Ru0b55Z5Z5Z5Z5Z5Z5Z5LCwsLCwsLCxnlnlnlnlnlnlnlnksLCwsLCwsLHBxTu2uXv4RY4hopu4+zgQC3iveBAfGCAdXwgQk=";

pub fn primed_aac_mkv() -> Vec<u8> {
    let mut output = STANDARD
        .decode(PRIMED_AAC_MKV_BASE64)
        .expect("checked-in fixture base64 is valid");
    // A legal top-level EBML Void keeps the decoded fixture near the requested
    // size without storing thousands of uninformative base64 zeroes. Expand
    // the segment's eight-byte size field so the Void remains inside it.
    output[48..52].copy_from_slice(&0x0fcc_u32.to_be_bytes());
    output.extend_from_slice(&[0xec, 0x4b, 0x81]);
    output.resize(4_096, 0);
    output
}
