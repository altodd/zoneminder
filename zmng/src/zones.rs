//! Detection zones and masks: named polygons in normalized coordinates
//! (0..1, so they fit any analysis resolution), each zone with its own
//! sensitivity. Stored per camera as JSON in `zones_json` / `masks_json`:
//!
//! ```json
//! [{"name": "Door", "points": [[0.1,0.2],[0.5,0.2],[0.5,0.9]], "min_area_pct": 0.5, "min_blob_pct": 0.2}]
//! ```
//!
//! `min_area_pct` (share of the zone's pixels that changed) and
//! `min_blob_pct` (share covered by the largest moving object) default to
//! the camera's values. The older form, bare polygons `[[[x,y],...],...]`,
//! is still read (its zones are called "Zone 1", "Zone 2", ...). Masks are
//! areas never analysed; only their points matter.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Most zones (and, separately, masks) per camera: the detector keeps one
/// bit per zone per pixel.
pub const MAX_ZONES: usize = 16;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    #[serde(default)]
    pub name: String,
    pub points: Vec<(f64, f64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_area_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_blob_pct: Option<f64>,
}

fn parse_any(json: &str) -> Option<Vec<Zone>> {
    let t = json.trim();
    if t.is_empty() {
        return Some(Vec::new());
    }
    if let Ok(z) = serde_json::from_str::<Vec<Zone>>(t) {
        return Some(z);
    }
    serde_json::from_str::<Vec<Vec<(f64, f64)>>>(t)
        .ok()
        .map(|polys| polys.into_iter().map(|points| Zone { name: String::new(), points, min_area_pct: None, min_blob_pct: None }).collect())
}

/// Zones (or masks) of a camera, unnamed ones called "`prefix` N". Invalid
/// JSON reads as none (the API refuses to store it, see [`validate`]).
pub fn parse(json: &str, prefix: &str) -> Vec<Zone> {
    let mut zones = parse_any(json).unwrap_or_default();
    for (i, z) in zones.iter_mut().enumerate() {
        if z.name.trim().is_empty() {
            z.name = format!("{prefix} {}", i + 1);
        }
    }
    zones
}

/// Just the polygons (object filtering, the ZoneMinder API).
pub fn parse_polys(json: &str) -> Vec<Vec<(f64, f64)>> {
    parse(json, "Zone").into_iter().map(|z| z.points).collect()
}

/// Check zones/masks JSON before it is stored: either form, at most
/// [`MAX_ZONES`], 3..=64 points within the frame, names of at most 40
/// characters without commas, unique per camera, sensitivities 0..100.
pub fn validate(json: &str, what: &str) -> Result<()> {
    let zones = parse_any(json).ok_or_else(|| anyhow::anyhow!("{what}: not a list of zones ([{{\"name\",\"points\":[[x,y],...]}}]) or polygons"))?;
    if zones.len() > MAX_ZONES {
        anyhow::bail!("{what}: at most {MAX_ZONES}");
    }
    let mut names: Vec<String> = Vec::new();
    for (i, z) in zones.iter().enumerate() {
        let label = if z.name.is_empty() { format!("#{}", i + 1) } else { format!("\"{}\"", z.name) };
        if !(3..=64).contains(&z.points.len()) {
            anyhow::bail!("{what} {label}: 3 to 64 points");
        }
        if z.points.iter().any(|(x, y)| !(0.0..=1.0).contains(x) || !(0.0..=1.0).contains(y)) {
            anyhow::bail!("{what} {label}: points must be inside the picture (0..1)");
        }
        let name = z.name.trim();
        if name.chars().count() > 40 || name.contains(',') {
            anyhow::bail!("{what} {label}: names are at most 40 characters, without commas");
        }
        if !name.is_empty() {
            if names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                anyhow::bail!("{what}: two are called \"{name}\"");
            }
            names.push(name.to_string());
        }
        for (k, v) in [("min_area_pct", z.min_area_pct), ("min_blob_pct", z.min_blob_pct)] {
            if v.is_some_and(|v| !(0.0..=100.0).contains(&v)) {
                anyhow::bail!("{what} {label}: {k} must be between 0 and 100");
            }
        }
    }
    Ok(())
}

/// Scanline polygon fill into a w*h mask (coords normalized 0..1): `f` is
/// applied to every covered pixel.
pub fn rasterize(w: usize, h: usize, poly: &[(f64, f64)], mut f: impl FnMut(usize)) {
    if poly.len() < 3 {
        return;
    }
    let pts: Vec<(f64, f64)> = poly.iter().map(|(x, y)| (x * w as f64, y * h as f64)).collect();
    for y in 0..h {
        let fy = y as f64 + 0.5;
        let mut xs: Vec<f64> = Vec::new();
        for i in 0..pts.len() {
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[(i + 1) % pts.len()];
            if (y0 <= fy && y1 > fy) || (y1 <= fy && y0 > fy) {
                xs.push(x0 + (fy - y0) * (x1 - x0) / (y1 - y0));
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        for pair in xs.chunks(2) {
            if pair.len() == 2 {
                let a = pair[0].max(0.0).round() as usize;
                let b = pair[1].min(w as f64).round() as usize;
                for x in a..b.min(w) {
                    f(y * w + x);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_forms_and_names_the_unnamed() {
        let old = parse("[[[0,0],[1,0],[1,1]],[[0,0],[0.5,0],[0.5,0.5]]]", "Zone");
        assert_eq!(old.len(), 2);
        assert_eq!(old[1].name, "Zone 2");
        assert_eq!(old[1].min_area_pct, None);
        let new = parse(r#"[{"name":"Door","points":[[0,0],[1,0],[1,1]],"min_area_pct":0.3},{"points":[[0,0],[1,0],[0,1]]}]"#, "Zone");
        assert_eq!(new[0].name, "Door");
        assert_eq!(new[0].min_area_pct, Some(0.3));
        assert_eq!(new[1].name, "Zone 2");
        assert!(parse("", "Mask").is_empty());
        assert!(parse("not json", "Zone").is_empty());
        assert_eq!(parse_polys(r#"[{"name":"a","points":[[0,0],[1,0],[1,1]]}]"#), vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]]);
        // round trip of the named form
        let back: Vec<Zone> = serde_json::from_str(&serde_json::to_string(&new).unwrap()).unwrap();
        assert_eq!(back[0], new[0]);
    }

    #[test]
    fn validation() {
        assert!(validate("[]", "zones").is_ok());
        assert!(validate("", "zones").is_ok());
        assert!(validate("[[[0,0],[1,0],[1,1]]]", "zones").is_ok());
        assert!(validate(r#"[{"name":"Door","points":[[0,0],[1,0],[1,1]],"min_blob_pct":2}]"#, "zones").is_ok());
        for bad in [
            "{}",
            r#"[{"name":"a","points":[[0,0],[1,0]]}]"#,
            r#"[{"name":"a","points":[[0,0],[1.5,0],[1,1]]}]"#,
            r#"[{"name":"a,b","points":[[0,0],[1,0],[1,1]]}]"#,
            r#"[{"name":"Door","points":[[0,0],[1,0],[1,1]]},{"name":"door","points":[[0,0],[1,0],[1,1]]}]"#,
            r#"[{"name":"a","points":[[0,0],[1,0],[1,1]],"min_area_pct":101}]"#,
        ] {
            assert!(validate(bad, "zones").is_err(), "{bad}");
        }
        let many: Vec<Zone> = (0..17).map(|i| Zone { name: format!("z{i}"), points: vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)], min_area_pct: None, min_blob_pct: None }).collect();
        assert!(validate(&serde_json::to_string(&many).unwrap(), "zones").is_err());
    }

    #[test]
    fn rasterizes_a_triangle() {
        let mut n = 0;
        rasterize(100, 100, &[(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)], |_| n += 1);
        assert!((4800..=5200).contains(&n), "{n}");
    }
}
