//! Static map of Kalshi temperature-market locations to their
//! observation airports' (lat, lon).
//!
//! Kalshi temperature markets settle on the NWS Climatological Report
//! for a specific airport — DEN = Denver International, LAX = Los
//! Angeles International, etc. Each entry is the **observation
//! station's** lat/lon (not the city center) because that's what
//! determines which NWS grid cell holds the forecast that the
//! settlement value is most-correlated with.
//!
//! Coverage is intentionally limited to Kalshi's actually-listed
//! locations (KXHIGHDEN, KXHIGHCHI, KXLAX..., etc.). Adding a city
//! requires adding both the airport coordinates AND verifying the
//! Kalshi series tag for the location matches the lookup key.
//!
//! Coordinates are 4-decimal-place — NWS rejects longer precision
//! with a 301 redirect to the truncated form.

#[derive(Debug, Clone, Copy)]
pub struct Airport {
    /// 3-letter Kalshi location code as it appears in series tickers
    /// (`"DEN"` from `KXHIGHDEN`, `"LAX"` from `KXLAX...`).
    pub code: &'static str,
    /// Human-readable city name for logging.
    pub city: &'static str,
    pub lat: f64,
    pub lon: f64,
    /// Iowa State Mesonet ASOS station identifier — the 3-letter
    /// code we use to pull historical realised observations from
    /// `mesonet.agron.iastate.edu`. For most airports this matches
    /// `code`, but Kalshi's location keys diverge in some cases
    /// (e.g. PHIL→PHL, NOLA→MSY, NY→LGA, DC→DCA, CHI→ORD). The
    /// table below sets the right value per row; lookups via
    /// [`Airport::asos_station_or_code`] fall back to `code` for
    /// the trivial cases.
    pub asos_station: &'static str,
}

impl Airport {
    /// ASOS station id, falling back to `code` if the field is
    /// empty (allowed for the common case where Kalshi's location
    /// key already matches the ASOS code).
    pub fn asos_station_or_code(&self) -> &'static str {
        if self.asos_station.is_empty() {
            self.code
        } else {
            self.asos_station
        }
    }
}

/// Lookup an airport by Kalshi 3-letter code. Case-insensitive.
/// Returns `None` for unmapped codes — caller should log a warning
/// and skip the market rather than fall through to a default.
pub fn lookup_airport(code: &str) -> Option<&'static Airport> {
    let upper = code.to_ascii_uppercase();
    AIRPORTS.iter().find(|a| a.code == upper)
}

/// Hand-curated. Coordinates from each airport's NWS station page;
/// truncated to 4 decimals.
pub const AIRPORTS: &[Airport] = &[
    Airport {
        code: "DEN",
        city: "Denver, CO",
        lat: 39.8617,
        lon: -104.6731,
        asos_station: "",
    },
    Airport {
        code: "LAX",
        city: "Los Angeles, CA",
        lat: 33.9416,
        lon: -118.4085,
        asos_station: "",
    },
    Airport {
        code: "NYC",
        city: "New York City, NY",
        lat: 40.7794,
        lon: -73.8803,
        asos_station: "LGA",
    }, // KLGA
    Airport {
        code: "MIA",
        city: "Miami, FL",
        lat: 25.7959,
        lon: -80.2870,
        asos_station: "",
    },
    Airport {
        code: "AUS",
        city: "Austin, TX",
        lat: 30.1975,
        lon: -97.6664,
        asos_station: "",
    },
    Airport {
        code: "CHI",
        city: "Chicago, IL",
        lat: 41.9742,
        lon: -87.9073,
        asos_station: "ORD",
    }, // KORD
    Airport {
        code: "PHX",
        city: "Phoenix, AZ",
        lat: 33.4342,
        lon: -112.0117,
        asos_station: "",
    },
    Airport {
        code: "DC",
        city: "Washington, DC",
        lat: 38.8521,
        lon: -77.0376,
        asos_station: "DCA",
    }, // KDCA
    Airport {
        code: "BOS",
        city: "Boston, MA",
        lat: 42.3656,
        lon: -71.0096,
        asos_station: "",
    },
    Airport {
        code: "SFO",
        city: "San Francisco, CA",
        lat: 37.6213,
        lon: -122.3790,
        asos_station: "",
    },
    Airport {
        code: "ATL",
        city: "Atlanta, GA",
        lat: 33.6367,
        lon: -84.4281,
        asos_station: "",
    },
    Airport {
        code: "DAL",
        city: "Dallas, TX",
        lat: 32.8998,
        lon: -97.0403,
        asos_station: "DFW",
    }, // KDFW
    Airport {
        code: "SEA",
        city: "Seattle, WA",
        lat: 47.4502,
        lon: -122.3088,
        asos_station: "",
    },
    Airport {
        code: "MSP",
        city: "Minneapolis, MN",
        lat: 44.8848,
        lon: -93.2223,
        asos_station: "",
    },
    Airport {
        code: "DTW",
        city: "Detroit, MI",
        lat: 42.2125,
        lon: -83.3533,
        asos_station: "",
    },
    Airport {
        code: "IAH",
        city: "Houston, TX",
        lat: 29.9844,
        lon: -95.3414,
        asos_station: "",
    },
    Airport {
        code: "PHL",
        city: "Philadelphia, PA",
        lat: 39.8744,
        lon: -75.2424,
        asos_station: "",
    },
    Airport {
        code: "TPA",
        city: "Tampa, FL",
        lat: 27.9755,
        lon: -82.5332,
        asos_station: "",
    },
    Airport {
        code: "PDX",
        city: "Portland, OR",
        lat: 45.5887,
        lon: -122.5975,
        asos_station: "",
    },
    Airport {
        code: "SAN",
        city: "San Diego, CA",
        lat: 32.7338,
        lon: -117.1933,
        asos_station: "",
    },
    Airport {
        code: "LAS",
        city: "Las Vegas, NV",
        lat: 36.0840,
        lon: -115.1537,
        asos_station: "",
    },
    Airport {
        code: "BNA",
        city: "Nashville, TN",
        lat: 36.1245,
        lon: -86.6782,
        asos_station: "",
    },
    Airport {
        code: "STL",
        city: "St. Louis, MO",
        lat: 38.7487,
        lon: -90.3700,
        asos_station: "",
    },
    Airport {
        code: "MCI",
        city: "Kansas City, MO",
        lat: 39.2976,
        lon: -94.7139,
        asos_station: "",
    },
    Airport {
        code: "BWI",
        city: "Baltimore, MD",
        lat: 39.1754,
        lon: -76.6683,
        asos_station: "",
    },
    Airport {
        code: "CVG",
        city: "Cincinnati, OH",
        lat: 39.0489,
        lon: -84.6678,
        asos_station: "",
    },
    Airport {
        code: "CLE",
        city: "Cleveland, OH",
        lat: 41.4117,
        lon: -81.8497,
        asos_station: "",
    },
    Airport {
        code: "MKE",
        city: "Milwaukee, WI",
        lat: 42.9472,
        lon: -87.8966,
        asos_station: "",
    },
    Airport {
        code: "OKC",
        city: "Oklahoma City, OK",
        lat: 35.3931,
        lon: -97.6007,
        asos_station: "",
    },
    Airport {
        code: "MEM",
        city: "Memphis, TN",
        lat: 35.0424,
        lon: -89.9767,
        asos_station: "",
    },
    // Kalshi uses these multi-letter codes:
    Airport {
        code: "PHIL",
        city: "Philadelphia, PA",
        lat: 39.8744,
        lon: -75.2424,
        asos_station: "PHL",
    }, // KPHL — same coords as PHL alias above
    Airport {
        code: "NOLA",
        city: "New Orleans, LA",
        lat: 29.9934,
        lon: -90.2580,
        asos_station: "MSY",
    }, // KMSY
    Airport {
        code: "JFK",
        city: "New York City, NY (JFK)",
        lat: 40.6398,
        lon: -73.7789,
        asos_station: "",
    },
    Airport {
        code: "BUF",
        city: "Buffalo, NY",
        lat: 42.9405,
        lon: -78.7322,
        asos_station: "",
    },
    Airport {
        code: "PIT",
        city: "Pittsburgh, PA",
        lat: 40.4914,
        lon: -80.2330,
        asos_station: "",
    },
    Airport {
        code: "IND",
        city: "Indianapolis, IN",
        lat: 39.7173,
        lon: -86.2944,
        asos_station: "",
    },
    Airport {
        code: "ORL",
        city: "Orlando, FL",
        lat: 28.4312,
        lon: -81.3081,
        asos_station: "",
    },
    Airport {
        code: "JAX",
        city: "Jacksonville, FL",
        lat: 30.4941,
        lon: -81.6879,
        asos_station: "",
    },
    Airport {
        code: "ABQ",
        city: "Albuquerque, NM",
        lat: 35.0402,
        lon: -106.6093,
        asos_station: "",
    },
    Airport {
        code: "SLC",
        city: "Salt Lake City, UT",
        lat: 40.7884,
        lon: -111.9778,
        asos_station: "",
    },
    // Aliases for codes Kalshi actually uses (often shorter or
    // different from the standard FAA 3-letter):
    Airport {
        code: "NY",
        city: "New York City, NY (LGA)",
        lat: 40.7794,
        lon: -73.8803,
        asos_station: "LGA",
    }, // alias for NYC
    Airport {
        code: "HOU",
        city: "Houston, TX (HOU/Hobby)",
        lat: 29.6454,
        lon: -95.2789,
        asos_station: "",
    }, // KHOU
    Airport {
        code: "LV",
        city: "Las Vegas, NV",
        lat: 36.0840,
        lon: -115.1537,
        asos_station: "LAS",
    }, // alias for LAS
    Airport {
        code: "MIN",
        city: "Minneapolis, MN",
        lat: 44.8848,
        lon: -93.2223,
        asos_station: "MSP",
    }, // alias for MSP
    Airport {
        code: "SATX",
        city: "San Antonio, TX",
        lat: 29.5337,
        lon: -98.4698,
        asos_station: "SAT",
    }, // KSAT
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_known_codes() {
        assert_eq!(lookup_airport("DEN").map(|a| a.city), Some("Denver, CO"));
        assert_eq!(
            lookup_airport("lax").map(|a| a.city),
            Some("Los Angeles, CA")
        );
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup_airport("ZZZ").is_none());
    }

    #[test]
    fn coordinates_truncated_to_4_decimals() {
        // NWS API rejects /points/{lat,lon} with more than 4 decimal
        // digits. Verify all coordinates round-trip cleanly through
        // the `:.4` formatter.
        for a in AIRPORTS {
            let lat_s = format!("{:.4}", a.lat);
            let lon_s = format!("{:.4}", a.lon);
            let lat_back: f64 = lat_s.parse().unwrap();
            let lon_back: f64 = lon_s.parse().unwrap();
            assert!(
                (lat_back - a.lat).abs() < 1e-6,
                "lat for {} not 4dp: {}",
                a.code,
                a.lat
            );
            assert!(
                (lon_back - a.lon).abs() < 1e-6,
                "lon for {} not 4dp: {}",
                a.code,
                a.lon
            );
        }
    }

    #[test]
    fn no_duplicate_codes() {
        let mut codes: Vec<&str> = AIRPORTS.iter().map(|a| a.code).collect();
        codes.sort_unstable();
        let n = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), n, "duplicate airport code in AIRPORTS");
    }

    #[test]
    fn codes_are_uppercase() {
        for a in AIRPORTS {
            assert!(
                a.code.chars().all(|c| c.is_ascii_uppercase()),
                "non-uppercase code: {}",
                a.code
            );
        }
    }
}
