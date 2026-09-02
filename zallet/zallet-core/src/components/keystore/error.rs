use std::fmt;

use abscissa_core::Application;

use crate::prelude::APP;

macro_rules! wfl {
    ($f:ident, $message_id:literal) => {
        write!($f, "{}", $crate::fl!($message_id))
    };

    ($f:ident, $message_id:literal, $($args:expr),* $(,)?) => {
        write!($f, "{}", $crate::fl!($message_id, $($args), *))
    };
}

#[allow(unused_macros)]
macro_rules! wlnfl {
    ($f:ident, $message_id:literal) => {
        writeln!($f, "{}", $crate::fl!($message_id))
    };

    ($f:ident, $message_id:literal, $($args:expr),* $(,)?) => {
        writeln!($f, "{}", $crate::fl!($message_id, $($args), *))
    };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeystoreError {
    MissingRecipients,
    /// The recipient set the keystore was asked to initialize with is empty.
    EmptyRecipients,
    /// An age recipients file contained an `@`-prefixed indirection entry, which cannot
    /// be stored as a recipient.
    RecipientIndirection(String),
    /// The provided passphrase did not decrypt the keystore's age identities.
    IncorrectPassphrase,
    /// The requested unlock timeout is large enough that the re-lock deadline would
    /// overflow [`std::time::SystemTime`].
    TimeoutTooLarge,
}

impl fmt::Display for KeystoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRecipients => {
                wlnfl!(f, "err-keystore-missing-recipients")?;
                wfl!(
                    f,
                    "rec-keystore-missing-recipients",
                    init_cmd = format!(
                        "zallet -d {} init-wallet-encryption",
                        APP.config().datadir().display()
                    )
                )
            }
            Self::EmptyRecipients => wfl!(f, "err-keystore-empty-recipients"),
            Self::RecipientIndirection(entry) => {
                wfl!(
                    f,
                    "err-keystore-recipient-indirection",
                    entry = entry.as_str()
                )
            }
            Self::IncorrectPassphrase => wfl!(f, "err-keystore-incorrect-passphrase"),
            Self::TimeoutTooLarge => wfl!(f, "err-keystore-timeout-too-large"),
        }
    }
}

impl std::error::Error for KeystoreError {}
