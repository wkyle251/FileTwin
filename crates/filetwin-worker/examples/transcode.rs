//! Development fixture helper; not used by the engine or the public CLI.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = args.next().ok_or("Input image required")?;
    let output = args.next().ok_or("Output image required")?;
    let decoded = image::open(input)?;
    match image::ImageFormat::from_path(&output)? {
        image::ImageFormat::Ico => decoded.into_rgba8().save(output)?,
        image::ImageFormat::Farbfeld => decoded.into_rgba16().save(output)?,
        image::ImageFormat::Hdr | image::ImageFormat::OpenExr => {
            decoded.into_rgb32f().save(output)?;
        }
        _ => decoded.save(output)?,
    }
    Ok(())
}
