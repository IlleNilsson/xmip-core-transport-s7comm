//! This technology's declaration: the transport guide's "The declaration",
//! `module/core/capability/transport/doc/adding-a-transport.md`.

use xcore::settings::{Applies, Kind, Presence, Setting, Settings};

/// Every setting a Location takes beyond its address, the CPU, `host:102`:
/// where a Location connects.
pub const SETTINGS: &Settings = &Settings {
    technology: env!("CARGO_PKG_NAME"),
    settings: &[
        Setting {
            name: "variable",
            kind: Kind::Text,
            presence: Presence::Required,
            meaning: "The S7 address a Receive Location polls and a Send Location writes \
                      when a target names none, such as `DB12.DBB0.100`.",
            applies: Applies::Both,
        },
        Setting {
            name: "rack",
            kind: Kind::Integer {
                minimum: 0,
                maximum: 7,
            },
            presence: Presence::Optional,
            meaning: "The rack the CPU sits in; rack 0 when left out.",
            applies: Applies::Both,
        },
        Setting {
            name: "slot",
            kind: Kind::Integer {
                minimum: 0,
                maximum: 31,
            },
            presence: Presence::Optional,
            meaning: "The slot the CPU sits in, slot 1 for 1200 and 1500 series; slot 2 \
                      when left out.",
            applies: Applies::Both,
        },
        transport::configured::timeout(
            Applies::Both,
            None,
            "How long a CPU that stops answering is waited on; unbounded when left \
             out.",
        ),
    ],
};

/// This technology's schemes, as [`transport::Configured::SCHEMES`] defines
/// them.
pub const SCHEMES: &[&str] = &["s7comm"];
