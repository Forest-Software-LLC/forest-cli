const LICENSE_CANDIDATES: [&str; 5] = [
    "LICENSE",
    "LICENSE.txt",
    "LICENSE.md",
    "COPYING",
    "COPYING.txt",
];

const MIT_LICENSE_TEXT: &str = r#"MIT License
Copyright (c) {YEAR} {HOLDER}
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, 
INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. 
IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, 
WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, 
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
"#;

const SPDX_LICENSES: [&str; 15] = [
    "MIT",
    "Apache-2.0",
    "GPL-3.0",
    "GPL-2.0",
    "MPL-2.0",
    "ISC",
    "Unlicense",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "LGPL-3.0",
    "LGPL-2.1",
    "AGPL-3.0",
    "EPL-2.0",
    "CDDL-1.0",
    "Zlib",
];


pub fn get_mit_license_text(copyright_holder: &str) -> String {
    MIT_LICENSE_TEXT
        .replace("{YEAR}", &chrono::Utc::now().format("%Y").to_string())
        .replace("{HOLDER}", copyright_holder)
}

fn find_license_file(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut found_license_path = None;
    for name in &LICENSE_CANDIDATES {
        let p = cwd.join(name);
        if p.exists() {
            found_license_path = Some(p);
            break;
        }
    }

    return found_license_path;
}

fn infer_license(text: &str) -> Option<&'static str> {
    let lc = text.to_ascii_lowercase();
    if lc.contains("mit license") {
        Some("MIT")
    } else if lc.contains("apache license") && lc.contains("version 2.0") {
        Some("Apache-2.0")
    } else if lc.contains("gnu general public license") && lc.contains("version 3") {
        Some("GPL-3.0")
    } else if lc.contains("gnu general public license") && lc.contains("version 2") {
        Some("GPL-2.0")
    } else if lc.contains("mozilla public license") && lc.contains("2.0") {
        Some("MPL-2.0")
    } else if lc.contains("isc license") || lc.contains("permission to use, copy, modify, and/or distribute this software for any purpose with or without fee") {
        Some("ISC")
    } else if lc.contains("the unlicense") || lc.contains("this is free and unencumbered software released into the public domain") {
        Some("Unlicense")
    } else if lc.contains("redistribution and use in source and binary forms") {
        // Very rough BSD heuristic
        if lc.contains("neither the name") || lc.contains("3-clause") {
            Some("BSD-3-Clause")
        } else {
            Some("BSD-2-Clause")
        }
    } else {
        None
    }
}


pub fn detect_license(cwd: &std::path::Path) -> Option<(String, bool)> {
    if let Some(license_path) = find_license_file(cwd) {
        if let Ok(text) = std::fs::read_to_string(&license_path) {
            if let Some(license) = infer_license(&text) {
                return Some((license.to_string(), true));
            } else {
                return Some((format!("SEE LICENSE IN {}", license_path.file_name().unwrap().to_string_lossy()), false));
            }
        }
    }
    None
}

pub fn sanitize_spdx(license: &str) -> &str {
    // Fix common mistakes such as lowercase, missing dashes, etc.
    let l = license.trim().to_uppercase();
    for &spdx in &SPDX_LICENSES {
        if l == spdx.to_uppercase() {
            return spdx;
        }
    }
    license
}