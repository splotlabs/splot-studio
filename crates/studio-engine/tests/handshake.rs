// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Process tests: spawn the real engine binary and speak the framed protocol.

use std::error::Error;
use std::io::Write;
use std::process::{Command as Process, Output, Stdio};

use studio_domain::protocol::{
    BuildIdentity, Command, Envelope, ErrorCode, Event, MAX_MESSAGE_BYTES, PROTOCOL_VERSION,
    read_message, write_message,
};

type TestResult = Result<(), Box<dyn Error>>;

const BIN: &str = env!("CARGO_BIN_EXE_splot-studio-engine");

/// Runs the engine with `args`, feeds it `stdin`, and waits for it to exit.
fn run(args: &[&str], stdin: &[u8]) -> Result<Output, Box<dyn Error>> {
    let mut child = Process::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // The engine may exit before reading everything (framing errors); that
    // is the behaviour under test, so a broken pipe here is not a failure.
    let _ = child.stdin.take().map(|mut pipe| pipe.write_all(stdin));
    Ok(child.wait_with_output()?)
}

fn handshake(stdin: &[u8]) -> Result<Output, Box<dyn Error>> {
    run(&["handshake"], stdin)
}

fn hello_frame(request_id: u64) -> Result<Vec<u8>, Box<dyn Error>> {
    let client = BuildIdentity {
        name: "test".into(),
        version: "0".into(),
    };
    let mut bytes = Vec::new();
    write_message(
        &mut bytes,
        &Envelope::new(request_id, Command::Hello { client }),
    )?;
    Ok(bytes)
}

fn raw_frame(body: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = u32::try_from(body.len())?.to_le_bytes().to_vec();
    bytes.extend_from_slice(body);
    Ok(bytes)
}

/// Decodes all of stdout as frames. Fails if any byte is not part of a frame,
/// which is how "stdout carries only protocol frames" is asserted.
fn events(output: &Output) -> Result<Vec<Envelope<Event>>, Box<dyn Error>> {
    let mut reader = output.stdout.as_slice();
    let mut events = Vec::new();
    while let Some(event) = read_message::<Event>(&mut reader)? {
        events.push(event);
    }
    Ok(events)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn hello_round_trip() -> TestResult {
    let output = handshake(&hello_frame(42)?)?;
    assert_eq!(output.status.code(), Some(0));
    let [reply] = events(&output)?
        .try_into()
        .map_err(|_| "expected one reply")?;
    assert_eq!(reply.protocol_version, PROTOCOL_VERSION);
    assert_eq!(reply.request_id, 42);
    let Event::Hello {
        engine,
        capabilities,
    } = reply.body
    else {
        return Err("expected Event::Hello".into());
    };
    assert_eq!(engine.name, "splot-studio-engine");
    assert_eq!(engine.version, env!("CARGO_PKG_VERSION"));
    assert!(
        capabilities.encode_outputs.is_empty(),
        "this build must not claim to encode"
    );
    Ok(())
}

#[test]
fn replies_follow_request_order() -> TestResult {
    let mut input = hello_frame(1)?;
    input.extend(hello_frame(2)?);
    let output = handshake(&input)?;
    assert_eq!(output.status.code(), Some(0));
    let ids: Vec<u64> = events(&output)?.iter().map(|e| e.request_id).collect();
    assert_eq!(ids, [1, 2]);
    Ok(())
}

#[test]
fn clean_eof_exits_zero_with_no_output() -> TestResult {
    let output = handshake(&[])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, Vec::<u8>::new());
    Ok(())
}

#[test]
fn version_mismatch_is_answered_and_the_session_continues() -> TestResult {
    let mut input = raw_frame(
        br#"{"protocol_version":999,"request_id":9,"body":{"type":"hello","client":{"name":"x","version":"0"}}}"#,
    )?;
    input.extend(hello_frame(10)?);
    let output = handshake(&input)?;
    assert_eq!(output.status.code(), Some(0));
    let got = events(&output)?;
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].request_id, 9);
    assert!(matches!(
        got[0].body,
        Event::Error {
            code: ErrorCode::IncompatibleVersion,
            ..
        }
    ));
    assert!(matches!(got[1].body, Event::Hello { .. }));
    Ok(())
}

#[test]
fn unknown_message_type_is_malformed() -> TestResult {
    let input = raw_frame(br#"{"protocol_version":1,"request_id":5,"body":{"type":"encode"}}"#)?;
    let output = handshake(&input)?;
    assert_eq!(output.status.code(), Some(0));
    let [reply] = events(&output)?
        .try_into()
        .map_err(|_| "expected one reply")?;
    assert_eq!(reply.request_id, 5);
    assert!(matches!(
        reply.body,
        Event::Error {
            code: ErrorCode::Malformed,
            ..
        }
    ));
    Ok(())
}

#[test]
fn undecodable_body_is_malformed_with_request_id_zero() -> TestResult {
    let output = handshake(&raw_frame(b"not json")?)?;
    assert_eq!(output.status.code(), Some(0));
    let [reply] = events(&output)?
        .try_into()
        .map_err(|_| "expected one reply")?;
    assert_eq!(reply.request_id, 0);
    assert!(matches!(
        reply.body,
        Event::Error {
            code: ErrorCode::Malformed,
            ..
        }
    ));
    Ok(())
}

#[test]
fn oversize_length_prefix_is_fatal() -> TestResult {
    let output = handshake(&(MAX_MESSAGE_BYTES + 1).to_le_bytes())?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, Vec::<u8>::new());
    assert!(
        stderr(&output).contains("outside 1..="),
        "stderr: {}",
        stderr(&output)
    );
    Ok(())
}

#[test]
fn zero_length_is_fatal() -> TestResult {
    let output = handshake(&0_u32.to_le_bytes())?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, Vec::<u8>::new());
    assert!(
        stderr(&output).contains("length 0"),
        "stderr: {}",
        stderr(&output)
    );
    Ok(())
}

#[test]
fn truncated_message_is_fatal() -> TestResult {
    let frame = hello_frame(1)?;
    // Inside the body, and inside the length prefix.
    for cut in [frame.len() - 1, 2] {
        let output = handshake(&frame[..cut])?;
        assert_eq!(output.status.code(), Some(1), "cut at {cut}");
        assert_eq!(output.stdout, Vec::<u8>::new(), "cut at {cut}");
        assert!(
            stderr(&output).contains("ended inside"),
            "stderr: {}",
            stderr(&output)
        );
    }
    Ok(())
}

#[test]
fn a_fatal_frame_after_a_good_one_keeps_the_earlier_reply() -> TestResult {
    let mut input = hello_frame(1)?;
    input.extend(0_u32.to_le_bytes());
    let output = handshake(&input)?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(events(&output)?.len(), 1);
    Ok(())
}

#[test]
fn help_is_truthful() -> TestResult {
    let output = run(&["--help"], &[])?;
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout)?;
    assert!(
        text.contains("handshake") && text.contains("cannot encode"),
        "{text}"
    );
    Ok(())
}

#[test]
fn version_reports_build_and_protocol_identity() -> TestResult {
    let output = run(&["--version"], &[])?;
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout)?;
    assert!(
        text.contains(&format!(
            "splot-studio-engine {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("protocol version: {PROTOCOL_VERSION}")),
        "{text}"
    );
    assert!(text.contains("encoding: not implemented"), "{text}");
    Ok(())
}

#[test]
fn unknown_argument_is_a_usage_error() -> TestResult {
    for args in [&["--bogus"][..], &[], &["handshake", "extra"]] {
        let output = run(args, &[])?;
        assert_eq!(output.status.code(), Some(2), "args {args:?}");
        assert_eq!(output.stdout, Vec::<u8>::new(), "args {args:?}");
        assert!(stderr(&output).contains("USAGE"), "args {args:?}");
    }
    Ok(())
}
