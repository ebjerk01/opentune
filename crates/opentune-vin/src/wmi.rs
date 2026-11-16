//! World Manufacturer Identifier (WMI) lookup.
//!
//! The first three characters of a VIN are the WMI (ISO 3780). This module
//! ships a *curated* table of the manufacturers OpenTune cares about most
//! (VAG, Mercedes-Benz, BMW) plus a representative spread of other common
//! makes, and a coarse country lookup keyed on the ISO 3780 region ranges.
//!
//! The table is intentionally partial and honest: a `None` manufacturer means
//! "not in our table", never "unknown WMI, here's a guess". Country, by
//! contrast, is derivable from the standardised first-one/first-two character
//! ranges, so it is attempted independently of whether the exact WMI is known.

/// What a WMI resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WmiInfo {
    /// The three-character WMI, upper-cased.
    pub wmi: String,
    /// Manufacturer/marque, if the WMI is in our curated table.
    pub manufacturer: Option<&'static str>,
    /// Assembly country/region, derived from the ISO 3780 character ranges.
    pub country: Option<&'static str>,
}

/// Curated WMI → manufacturer table. Order does not matter; lookup is exact on
/// the three-character prefix. Kept small and verifiable rather than
/// exhaustive — the goal is confident coverage of the makes we tune, not a
/// copy of every WMI ever issued.
const WMI_TABLE: &[(&str, &str)] = &[
    // ---- Volkswagen Audi Group (VAG) ----
    ("WVW", "Volkswagen (passenger)"),
    ("WV1", "Volkswagen Commercial Vehicles"),
    ("WV2", "Volkswagen Bus/Van"),
    ("WV3", "Volkswagen Commercial Vehicles"),
    ("WVG", "Volkswagen SUV"),
    ("1VW", "Volkswagen (USA)"),
    ("3VW", "Volkswagen (Mexico)"),
    ("9BW", "Volkswagen (Brazil)"),
    ("WAU", "Audi"),
    ("WA1", "Audi SUV"),
    ("WUA", "Audi Sport (quattro GmbH)"),
    ("TRU", "Audi (Hungary)"),
    ("TMB", "Škoda"),
    ("VSS", "SEAT"),
    // ---- Mercedes-Benz / Daimler ----
    ("WDB", "Mercedes-Benz"),
    ("WDC", "Mercedes-Benz (SUV/M-class)"),
    ("WDD", "Mercedes-Benz (passenger)"),
    ("WDF", "Mercedes-Benz (van/transporter)"),
    ("WMX", "Mercedes-AMG"),
    ("W1K", "Mercedes-Benz (passenger, post-2021 WMI)"),
    ("W1N", "Mercedes-Benz (SUV, post-2021 WMI)"),
    ("W1V", "Mercedes-Benz (van/transporter, post-2021 WMI)"),
    ("W1T", "Mercedes-Benz (truck)"),
    ("4JG", "Mercedes-Benz (USA, SUV)"),
    // ---- BMW ----
    ("WBA", "BMW (passenger)"),
    ("WBS", "BMW M GmbH"),
    ("WBX", "BMW (SUV)"),
    ("WBY", "BMW i / electric"),
    ("4US", "BMW (USA)"),
    ("5UX", "BMW (USA, SUV)"),
    ("5YM", "BMW M (USA)"),
    // ---- Ford ----
    ("1FA", "Ford (USA, passenger)"),
    ("1FB", "Ford (USA, bus)"),
    ("1FC", "Ford (USA)"),
    ("1FD", "Ford (USA, incomplete/truck)"),
    ("1FM", "Ford (USA, MPV/SUV)"),
    ("1FT", "Ford (USA, truck)"),
    ("WF0", "Ford (Europe)"),
    ("WF1", "Ford (Europe)"),
    // ---- Toyota / Lexus ----
    ("JTD", "Toyota (Japan)"),
    ("JTM", "Toyota (Japan)"),
    ("JTN", "Toyota (Japan)"),
    ("JTH", "Lexus (Japan)"),
    ("4T1", "Toyota (USA)"),
    ("4T3", "Toyota (USA, SUV)"),
    ("5TD", "Toyota (USA)"),
    ("5TF", "Toyota (USA, truck)"),
    ("2T1", "Toyota (Canada)"),
    // ---- A few other high-volume makes ----
    ("VF1", "Renault"),
    ("VF3", "Peugeot"),
    ("VF7", "Citroën"),
    ("ZFA", "Fiat"),
    ("ZAR", "Alfa Romeo"),
    ("SAJ", "Jaguar"),
    ("SAL", "Land Rover"),
    ("SAR", "Rover"),
    ("VSK", "Nissan (Spain)"),
    ("JN1", "Nissan (Japan)"),
    ("SJN", "Nissan (UK)"),
    ("KMH", "Hyundai"),
    ("KNA", "Kia"),
    ("KND", "Kia (SUV)"),
    ("YV1", "Volvo (passenger)"),
    ("YS3", "Saab"),
    ("WP0", "Porsche (passenger)"),
    ("WP1", "Porsche (SUV)"),
    ("VWV", "Volkswagen (SKD/assembly)"),
];

/// Look up a WMI (first 3 chars of a VIN). `wmi` may be any case; it is
/// normalised. Always returns a [`WmiInfo`]; `manufacturer`/`country` are
/// `None` when not determinable.
pub fn lookup_wmi(wmi: &str) -> WmiInfo {
    let w: String = wmi.trim().to_ascii_uppercase();
    let manufacturer = WMI_TABLE
        .iter()
        .find(|(k, _)| *k == w)
        .map(|(_, name)| *name);
    let country = country_from_wmi(&w);
    WmiInfo {
        wmi: w,
        manufacturer,
        country,
    }
}

/// Coarse assembly-country lookup from the ISO 3780 region ranges. Uses the
/// first character for the region and the first two characters for the
/// country band. Returns `None` for ranges we do not map rather than guessing.
///
/// The ranges below are the standardised, published ISO 3780 allocations; this
/// covers the regions the shipped WMI table draws from, not every country.
fn country_from_wmi(w: &str) -> Option<&'static str> {
    let b = w.as_bytes();
    if b.len() < 2 {
        return None;
    }
    let c0 = b[0] as char;
    let c1 = b[1] as char;

    match c0 {
        // --- Africa: A-H ---
        'A'..='H' => None, // not mapped at country granularity here
        // --- Asia: J-R ---
        'J' => Some("Japan"),
        'K' => match c1 {
            'L'..='R' => Some("South Korea"),
            'A'..='E' => Some("Sri Lanka"),
            'F'..='K' => Some("Israel"),
            'S'..='Z' => Some("Kazakhstan"),
            _ => None,
        },
        'L' => Some("China"),
        'M' => match c1 {
            'A'..='E' => Some("India"),
            'F'..='K' => Some("Indonesia"),
            'L'..='R' => Some("Thailand"),
            _ => None,
        },
        'N' => match c1 {
            'F'..='K' => Some("Pakistan"),
            'L'..='R' => Some("Turkey"),
            _ => None,
        },
        'P' => match c1 {
            'A'..='E' => Some("Philippines"),
            'F'..='K' => Some("Singapore"),
            'L'..='R' => Some("Malaysia"),
            _ => None,
        },
        'R' => match c1 {
            'F'..='K' => Some("Taiwan"),
            'L'..='R' => Some("Vietnam"),
            _ => None,
        },
        // --- Europe: S-Z ---
        'S' => match c1 {
            'A'..='M' => Some("United Kingdom"),
            'N'..='T' => Some("Germany"),
            'U'..='Z' => Some("Poland"),
            _ => None,
        },
        'T' => match c1 {
            'A'..='H' => Some("Switzerland"),
            'J'..='P' => Some("Czech Republic"),
            'R'..='V' => Some("Hungary"),
            'W'..='Z' => Some("Portugal"),
            _ => None,
        },
        'V' => match c1 {
            'A'..='E' => Some("Austria"),
            'F'..='R' => Some("France"),
            'S'..='W' => Some("Spain"),
            'X'..='Z' => Some("Serbia"),
            _ => None,
        },
        'W' => Some("Germany"),
        'X' => match c1 {
            'L'..='R' => Some("Netherlands"),
            'S'..='W' => Some("USSR/CIS"),
            _ => None,
        },
        'Y' => match c1 {
            'A'..='E' => Some("Belgium"),
            'F'..='K' => Some("Finland"),
            'S'..='W' => Some("Sweden"),
            _ => None,
        },
        'Z' => match c1 {
            'A'..='R' => Some("Italy"),
            _ => None,
        },
        // --- North America: 1-5 ---
        '1' | '4' | '5' => Some("United States"),
        '2' => Some("Canada"),
        '3' => Some("Mexico"),
        // --- South America: 8-9 ---
        '8' => None,
        '9' => match c1 {
            'A'..='E' => Some("Brazil"),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vag_wmi() {
        let i = lookup_wmi("wvw");
        assert_eq!(i.wmi, "WVW");
        assert_eq!(i.manufacturer, Some("Volkswagen (passenger)"));
        assert_eq!(i.country, Some("Germany"));
    }

    #[test]
    fn mercedes_and_bmw() {
        assert_eq!(lookup_wmi("WDB").manufacturer, Some("Mercedes-Benz"));
        assert_eq!(lookup_wmi("WDD").country, Some("Germany"));
        assert_eq!(lookup_wmi("WBA").manufacturer, Some("BMW (passenger)"));
    }

    #[test]
    fn country_without_manufacturer() {
        // A plausible but un-tabled WMI still yields a country from the range.
        let i = lookup_wmi("JA3");
        assert_eq!(i.manufacturer, None);
        assert_eq!(i.country, Some("Japan"));
    }

    #[test]
    fn unknown_is_none_not_guess() {
        let i = lookup_wmi("ZZZ");
        assert_eq!(i.manufacturer, None);
        // 'Z' with 'Z' second char is outside the mapped Italy band.
        assert_eq!(i.country, None);
    }
}
