// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The processor floor of the engine this app was packaged with.
//!
//! A release engine for x86-64 is compiled for `x86-64-v3`
//! (`-C target-cpu=x86-64-v3`, set by `cargo xtask package` for the engine
//! only). It may use those instructions anywhere, including before its own
//! `main`, so it cannot check the processor itself. The app is compiled for
//! plain x86-64 and checks, with the standard library's run-time detection,
//! before it starts anything else; on a processor below the floor it says so
//! and exits without ever starting the engine.
//!
//! The packager states the engine's floor at compile time through
//! `SPLOT_STUDIO_ENGINE_CPU`. A development build (`cargo run`, `cargo xtask
//! run`) has a portable engine and no floor.

/// The engine's processor floor, as the packager stated it.
pub const ENGINE_CPU: Option<&str> = option_env!("SPLOT_STUDIO_ENGINE_CPU");

/// The `x86-64-v3` feature set as `rustc 1.100 --print cfg -C
/// target-cpu=x86-64-v3` lists it (`cargo xtask package` checks that the two
/// agree). `is_x86_feature_detected!` also checks that the operating system
/// saves the AVX register state, which the `xsave` bit alone does not prove.
pub const X86_64_V3: [&str; 18] = [
    "avx",
    "avx2",
    "bmi1",
    "bmi2",
    "cmpxchg16b",
    "f16c",
    "fma",
    "fxsr",
    "lzcnt",
    "movbe",
    "popcnt",
    "sse",
    "sse2",
    "sse3",
    "sse4.1",
    "sse4.2",
    "ssse3",
    "xsave",
];

/// The features the engine needs; empty when it has no floor.
#[must_use]
pub fn required() -> &'static [&'static str] {
    match ENGINE_CPU {
        Some("x86-64-v3") if cfg!(target_arch = "x86_64") => &X86_64_V3,
        _ => &[],
    }
}

/// The required features this processor lacks.
#[must_use]
pub fn missing() -> Vec<&'static str> {
    required()
        .iter()
        .copied()
        .filter(|feature| !detected(feature))
        .collect()
}

#[cfg(target_arch = "x86_64")]
fn detected(feature: &str) -> bool {
    // The macro needs a literal; an unknown name is never "present".
    match feature {
        "avx" => std::arch::is_x86_feature_detected!("avx"),
        "avx2" => std::arch::is_x86_feature_detected!("avx2"),
        "bmi1" => std::arch::is_x86_feature_detected!("bmi1"),
        "bmi2" => std::arch::is_x86_feature_detected!("bmi2"),
        "cmpxchg16b" => std::arch::is_x86_feature_detected!("cmpxchg16b"),
        "f16c" => std::arch::is_x86_feature_detected!("f16c"),
        "fma" => std::arch::is_x86_feature_detected!("fma"),
        "fxsr" => std::arch::is_x86_feature_detected!("fxsr"),
        "lzcnt" => std::arch::is_x86_feature_detected!("lzcnt"),
        "movbe" => std::arch::is_x86_feature_detected!("movbe"),
        "popcnt" => std::arch::is_x86_feature_detected!("popcnt"),
        "sse" => std::arch::is_x86_feature_detected!("sse"),
        "sse2" => std::arch::is_x86_feature_detected!("sse2"),
        "sse3" => std::arch::is_x86_feature_detected!("sse3"),
        "sse4.1" => std::arch::is_x86_feature_detected!("sse4.1"),
        "sse4.2" => std::arch::is_x86_feature_detected!("sse4.2"),
        "ssse3" => std::arch::is_x86_feature_detected!("ssse3"),
        "xsave" => std::arch::is_x86_feature_detected!("xsave"),
        _ => false,
    }
}

#[cfg(not(target_arch = "x86_64"))]
const fn detected(_feature: &str) -> bool {
    false
}

/// The notice shown instead of the window: a heading and an explanation.
#[must_use]
pub fn notice(missing: &[&str]) -> (String, String) {
    (
        "This processor is not supported".to_owned(),
        format!(
            "Splot Studio needs a 64-bit processor with the x86-64-v3 instruction set \
             (AVX2, FMA, BMI1/BMI2, F16C, LZCNT, MOVBE), found in Intel processors from \
             2013 (Haswell) and AMD processors from 2015 (Excavator) onwards. This one \
             lacks: {}. Nothing was started and no file was changed.",
            missing.join(", ")
        ),
    )
}

/// One line for `--version`.
#[must_use]
pub fn describe() -> String {
    match (ENGINE_CPU, required()) {
        (None, _) => "engine CPU floor: none (the engine is built for this target's default \
                      processor; a development build is portable)"
            .to_owned(),
        (Some(floor), []) => format!("engine CPU floor: {floor} (not checked on this target)"),
        (Some(floor), features) => {
            let missing = missing();
            let verdict = if missing.is_empty() {
                "this CPU has all of them".to_owned()
            } else {
                format!("this CPU lacks {}", missing.join(", "))
            };
            format!(
                "engine CPU floor: {floor}; checked: {}; {verdict}",
                features.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_v3_list_has_no_duplicates_and_every_name_is_known() {
        let mut sorted = X86_64_V3.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), X86_64_V3.len());
        // The baseline features every x86-64 processor has must be detected
        // through the same match, so a typo in a name cannot hide.
        if cfg!(target_arch = "x86_64") {
            for feature in ["sse", "sse2", "fxsr"] {
                assert!(detected(feature), "{feature}");
            }
        }
        assert!(!detected("not-a-feature"));
    }

    #[test]
    fn the_notice_names_what_is_missing() {
        let (heading, message) = notice(&["avx2", "fma"]);
        assert!(heading.contains("not supported"));
        assert!(message.contains("lacks: avx2, fma."), "{message}");
    }
}
