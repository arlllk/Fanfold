//! Any username/password: a successful render always has exactly the six expected
//! lines, so no secret can inject configuration.
#![no_main]

use fanfold_server::ini::render_personal_ini;
use libfuzzer_sys::fuzz_target;
use secrecy::{ExposeSecret, SecretString};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let (user, pass) = text.split_once('\u{0}').unwrap_or((&text, "x"));
    let user = SecretString::from(user.to_owned());
    let pass = SecretString::from(pass.to_owned());
    if let Ok(ini) = render_personal_ini(Some((&user, &pass)), None) {
        assert_eq!(ini.expose_secret().lines().count(), 6);
    }
});
