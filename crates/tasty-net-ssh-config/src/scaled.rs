// This file is derived from OpenSSH, modified: a Rust port of scan_scaled of
// openbsd-compat/fmt_scaled.c of OpenSSH portable 10.6p2 (commit
// d449e662b6b8b8cb20e852d8084c269af1c5e8f1). The notice of that file follows as written
// there.
//
// openbsd-compat/fmt_scaled.c:
// /*
//  * Copyright (c) 2001, 2002, 2003 Ian F. Darwin.  All rights reserved.
//  *
//  * Redistribution and use in source and binary forms, with or without
//  * modification, are permitted provided that the following conditions
//  * are met:
//  * 1. Redistributions of source code must retain the above copyright
//  *    notice, this list of conditions and the following disclaimer.
//  * 2. Redistributions in binary form must reproduce the above copyright
//  *    notice, this list of conditions and the following disclaimer in the
//  *    documentation and/or other materials provided with the distribution.
//  * 3. The name of the author may not be used to endorse or promote products
//  *    derived from this software without specific prior written permission.
//  *
//  * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
//  * IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//  * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
//  * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT,
//  * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
//  * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//  * DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//  * THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//  * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
//  * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! Numbers with a binary unit for `RekeyLimit` (`1G`, `1.5M`, `512`): `scan_scaled`
//! (`openbsd-compat/fmt_scaled.c:75`–`:229`). It follows the original step by step (the
//! `MAX_DIGITS` bound, the fraction digits it keeps, and the order of the multiplications by
//! 1024 and the divisions by 10), so it keeps the notice of that file. The behavior: leading
//! ASCII whitespace, at most one sign, decimal digits with an optional fraction, then an
//! optional unit `B`, `K`, `M`, `G`, `T`, `P` or `E` (powers of 1024, either case) that must
//! not be followed by a letter or digit. Without a unit the fraction is dropped.

/// The units in order of their power of 1024.
const UNITS: &[u8] = b"BKMGTPE";
/// The base of the units.
const UNIT_BASE: i64 = 1024;
/// Decimal base.
const TEN: i64 = 10;
/// The whole part may have fewer digits than this; extra fraction digits are ignored from
/// this length on (`MAX_DIGITS`, three times the number of units).
const MAX_DIGITS: usize = 21;

/// The value of `s`, or `None` if it is not a valid scaled number or does not fit an `i64`.
pub(crate) fn scan_scaled(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = b
        .iter()
        .take_while(|c| c.is_ascii_whitespace() || **c == 0x0b)
        .count();
    let mut sign: Option<bool> = None;
    while let Some(&c) = b.get(i) {
        let negative = match c {
            b'-' => true,
            b'+' => false,
            _ => break,
        };
        if sign.is_some() {
            return None;
        }
        sign = Some(negative);
        i += 1;
    }
    let mut whole: i64 = 0;
    let mut whole_digits = 0usize;
    let mut fraction: i64 = 0;
    // 0 before a '.', then one more than the number of fraction digits kept.
    let mut fraction_len = 0usize;
    while let Some(&c) = b.get(i) {
        if c == b'.' {
            if fraction_len > 0 {
                return None;
            }
            fraction_len = 1;
            i += 1;
            continue;
        }
        if !c.is_ascii_digit() {
            break;
        }
        let d = i64::from(c - b'0');
        if fraction_len > 0 {
            if fraction_len < MAX_DIGITS - 1 {
                fraction_len += 1;
                fraction = fraction.checked_mul(TEN)?.checked_add(d)?;
            }
        } else {
            whole_digits += 1;
            if whole_digits >= MAX_DIGITS {
                return None;
            }
            whole = whole.checked_mul(TEN)?.checked_add(d)?;
        }
        i += 1;
    }
    let negative = sign == Some(true);
    if negative {
        whole = -whole;
    }
    let Some(&unit) = b.get(i) else {
        return Some(whole);
    };
    let power = UNITS.iter().position(|u| *u == unit.to_ascii_uppercase())?;
    if b.get(i + 1).is_some_and(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let factor = UNIT_BASE.checked_pow(u32::try_from(power).ok()?)?;
    whole = whole.checked_mul(factor)?;
    // fraction * factor / 10^(fraction_len - 1), multiplying first while it fits.
    let mut muls = power;
    let mut divs = fraction_len.saturating_sub(1);
    while muls > 0 && divs > 0 {
        if fraction <= i64::MAX / UNIT_BASE {
            fraction *= UNIT_BASE;
            muls -= 1;
        } else {
            fraction /= TEN;
            divs -= 1;
        }
    }
    for _ in 0..muls {
        fraction = fraction.wrapping_mul(UNIT_BASE);
    }
    for _ in 0..divs {
        fraction /= TEN;
    }
    Some(if negative {
        whole - fraction
    } else {
        whole + fraction
    })
}

#[cfg(test)]
mod tests {
    use super::scan_scaled;

    #[test]
    fn values() {
        assert_eq!(scan_scaled("512"), Some(512));
        assert_eq!(scan_scaled("1K"), Some(1024));
        assert_eq!(scan_scaled("1k"), Some(1024));
        assert_eq!(scan_scaled("1.5M"), Some(1_572_864));
        assert_eq!(scan_scaled("1.5"), Some(1));
        assert_eq!(scan_scaled(" +2G"), Some(2_147_483_648));
        assert_eq!(scan_scaled("-1K"), Some(-1024));
        assert_eq!(scan_scaled("1KB"), None);
        assert_eq!(scan_scaled("1X"), None);
        assert_eq!(scan_scaled("1..2"), None);
        assert_eq!(scan_scaled("+-1"), None);
        assert_eq!(scan_scaled("16E"), None);
        assert_eq!(scan_scaled("1K!"), Some(1024));
    }
}
