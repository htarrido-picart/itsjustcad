// icon.rs — shared .ico synthesis used by build.rs (Windows resource
// embedding) and by the icon_assets integration test.
//
// This file is pulled in via `include!`, never compiled standalone, so it must
// stay self-contained: no `crate::` paths, only the `ico` crate and std.

/// Build a multi-size Windows .ico from PNG byte slices.
///
/// Each input must be a valid PNG (ideally square, power-of-two sizes such as
/// 16/32/64/128/256). Returns the serialized ICO bytes containing one entry
/// per input, or an error if an input fails to decode or no inputs are given.
pub fn build_ico(pngs: &[&[u8]]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if pngs.is_empty() {
        return Err("build_ico: no PNG inputs given".into());
    }
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for png in pngs {
        let image = ico::IconImage::read_png(*png)?;
        dir.add_entry(ico::IconDirEntry::encode(&image)?);
    }
    let mut out = Vec::new();
    dir.write(&mut out)?;
    Ok(out)
}
