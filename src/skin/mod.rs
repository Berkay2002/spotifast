//! Classic Winamp skins.
//!
//! A `.wsz` file contains bitmap sprite sheets and two small text files. This
//! module decodes them to RGBA textures. [`sprites`] defines source coordinates;
//! [`layout`] defines window positions. Missing files fall back to Spotifast's
//! built-in classic skin. Modern `.wal` skins are unsupported.

pub mod config;
pub mod font;
pub mod layout;
pub mod sprites;
pub mod zip;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, LazyLock};

use thiserror::Error;

pub use config::{Mask, PlaylistStyle, Regions, Rgb, VisColors};
pub use sprites::{Sheet, Sprite};

#[derive(Debug, Error)]
pub enum SkinError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("not a zip archive; a classic skin is a .wsz file or a folder of bitmaps")]
    NotAnArchive,
    #[error("{0}")]
    Archive(zip::ZipError),
    #[error("this is a modern Winamp skin, which Spotifast cannot draw; it needs a classic one")]
    ModernSkin,
    #[error("no skin bitmaps were found inside")]
    Empty,
}

/// A decoded bitmap: RGBA, rows top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Bitmap {
    /// Decodes a BMP or PNG, whatever the file was called: skins mislabel
    /// them. Anything unreadable is `None`, as Winamp silently skipped it.
    /// Real sheets are a few hundred pixels a side, so one far larger than
    /// that is skipped too, rather than inflating to gigabytes of pixels.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .ok()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_SHEET_SIDE);
        limits.max_image_height = Some(MAX_SHEET_SIDE);
        limits.max_alloc = Some(MAX_SHEET_BYTES);
        reader.limits(limits);
        let image = reader.decode().ok()?.into_rgba8();
        Some(Self {
            width: image.width(),
            height: image.height(),
            rgba: image.into_raw(),
        })
    }

    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let at = 4 * (y * self.width + x) as usize;
        self.rgba[at..at + 4].try_into().ok()
    }

    /// A copy of the part of this bitmap a sprite covers, clipped to it.
    pub fn crop(&self, sprite: Sprite) -> Option<Bitmap> {
        let sprite = sprite.clipped_to(self.width, self.height)?;
        let mut rgba = Vec::with_capacity(4 * (sprite.width * sprite.height) as usize);
        for y in sprite.y..sprite.y + sprite.height {
            let start = 4 * (y * self.width + sprite.x) as usize;
            rgba.extend_from_slice(&self.rgba[start..start + 4 * sprite.width as usize]);
        }
        Some(Bitmap {
            width: sprite.width,
            height: sprite.height,
            rgba,
        })
    }
}

/// The files a skin is read from, keyed by lower-case file name.
type Files = HashMap<String, Vec<u8>>;

/// The most a whole skin may hold: the file read from disk, and everything
/// inflated out of it. A classic skin is well under a megabyte.
const SKIN_LIMIT: usize = 32 << 20;

/// Refuses a file larger than any skin before it is read into memory.
fn check_size(path: &Path, len: u64) -> Result<(), SkinError> {
    if len > SKIN_LIMIT as u64 {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        return Err(SkinError::Archive(zip::ZipError::TooLarge(name.into())));
    }
    Ok(())
}

pub struct Skin {
    /// The file or folder name, for showing which skin is on.
    pub name: String,
    sheets: HashMap<Sheet, Bitmap>,
    pub playlist: PlaylistStyle,
    pub vis_colors: VisColors,
    /// The windows' shapes, for skins that are not rectangles.
    pub regions: Regions,
}

impl Skin {
    /// Reads a `.wsz` file or an unpacked skin folder.
    pub fn load(path: &Path) -> Result<Self, SkinError> {
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            Self::from_dir(name, path)
        } else {
            check_size(path, std::fs::metadata(path)?.len())?;
            Self::from_archive(name, &std::fs::read(path)?)
        }
    }

    /// Reads a `.wsz` archive. File names are matched without regard to
    /// case or folder, and when a name repeats the last copy wins, which
    /// is what unpacking onto Winamp's file system produced.
    pub fn from_archive(name: impl Into<String>, bytes: &[u8]) -> Result<Self, SkinError> {
        let name = name.into();
        let archive = zip::Archive::parse(bytes).map_err(|error| match error {
            zip::ZipError::NotAnArchive => SkinError::NotAnArchive,
            other => SkinError::Archive(other),
        })?;
        let mut files = Files::new();
        let mut tried = HashSet::new();
        let mut total = 0;
        // Walking backwards and keeping the first copy is the same as the
        // last copy winning, but inflates each name once, whether or not it
        // reads: a crafted archive can point thousands of entries at one
        // large stream that fails only once it is inflated.
        for entry in archive.entries().iter().rev() {
            if entry.is_dir() {
                continue;
            }
            let file_name = entry.file_name();
            if !wanted(&file_name) || !tried.insert(file_name.clone()) {
                continue;
            }
            match archive.read(entry) {
                Ok(bytes) => {
                    total += bytes.len();
                    if total > SKIN_LIMIT {
                        return Err(SkinError::Archive(zip::ZipError::TooLarge(
                            entry.name.clone(),
                        )));
                    }
                    files.insert(file_name, bytes);
                }
                Err(error) => log::warn!("skin {name}: {error}"),
            }
        }
        Self::from_files(name, files)
    }

    /// Reads an unpacked skin: a folder with the bitmaps in it.
    /// Nested folders are searched breadth first, without following links.
    /// The nearest copy wins; equal-depth paths are read in name order.
    pub fn from_dir(name: impl Into<String>, dir: &Path) -> Result<Self, SkinError> {
        let mut files = Files::new();
        let mut total = 0;
        let mut folders = VecDeque::from([(dir.to_path_buf(), 0)]);
        while let Some((folder, depth)) = folders.pop_front() {
            let mut entries = std::fs::read_dir(folder)?.collect::<Result<Vec<_>, _>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let kind = entry.file_type()?;
                if kind.is_dir() && depth < MAX_SKIN_DEPTH {
                    folders.push_back((entry.path(), depth + 1));
                } else if kind.is_file() {
                    let file_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                    if wanted(&file_name) && !files.contains_key(&file_name) {
                        check_size(&entry.path(), entry.metadata()?.len())?;
                        let bytes = std::fs::read(entry.path())?;
                        total += bytes.len();
                        check_size(&entry.path(), total as u64)?;
                        files.insert(file_name, bytes);
                    }
                }
            }
        }
        Self::from_files(name.into(), files)
    }

    fn from_files(name: String, files: Files) -> Result<Self, SkinError> {
        let mut sheets = HashMap::new();
        for sheet in Sheet::ALL {
            let stem = sheet.file_stem();
            let bytes = files
                .get(&format!("{stem}.bmp"))
                .or_else(|| files.get(&format!("{stem}.png")));
            let Some(bytes) = bytes else {
                continue;
            };
            match Bitmap::decode(bytes) {
                Some(bitmap) => {
                    sheets.insert(sheet, bitmap);
                }
                None => log::warn!("skin {name}: {stem} could not be decoded and is skipped"),
            }
        }
        if sheets.is_empty() {
            return Err(if files.contains_key("skin.xml") {
                SkinError::ModernSkin
            } else {
                SkinError::Empty
            });
        }
        let text = |file: &str| {
            files.get(file).map(|bytes| {
                // A skin edited on Windows carries a byte order mark, and U+FEFF is not
                // whitespace, so `trim` leaves it on the first line: `[Text]` stops being a
                // section header and the first `r,g,b` stops being a colour.
                let text = String::from_utf8_lossy(bytes);
                text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
            })
        };
        let playlist = text("pledit.txt")
            .map(|text| PlaylistStyle::parse(&text))
            .unwrap_or_default();
        let vis_colors = text("viscolor.txt")
            .map(|text| config::parse_vis_colors(&text))
            .unwrap_or(config::DEFAULT_VIS_COLORS);
        let regions = text("region.txt")
            .map(|text| config::parse_regions(&text))
            .unwrap_or_default();
        Ok(Self {
            name,
            sheets,
            playlist,
            vis_colors,
            regions,
        })
    }

    /// The skin Spotifast ships, drawn for it and packed as a `.wsz` like
    /// any other, so it goes through the same reader. It has every sheet,
    /// so any other skin's gaps can be filled from it.
    pub fn builtin() -> Arc<Skin> {
        BUILTIN.clone()
    }

    /// Whether the skin brought this sheet itself.
    pub fn has(&self, sheet: Sheet) -> bool {
        self.sheets.contains_key(&sheet)
    }

    /// Whether the time display can take its blank cell and minus sign
    /// from the skin's own digits, rather than borrowing a bar of the 2.
    pub fn has_extended_digits(&self) -> bool {
        self.has(Sheet::NumsEx)
    }

    /// The bitmap for a sheet: the skin's own, or what stands in for a
    /// missing one. Balance borrows the volume sheet, whose middle the
    /// balance frames are cut from, as Winamp did; digits come from
    /// whichever digit sheet the skin has; anything else comes from the
    /// built-in skin.
    pub fn sheet(&self, sheet: Sheet) -> &Bitmap {
        if let Some(bitmap) = self.sheets.get(&sheet) {
            return bitmap;
        }
        let substitute = match sheet {
            Sheet::Balance => Sheet::Volume,
            Sheet::Numbers => Sheet::NumsEx,
            Sheet::NumsEx => Sheet::Numbers,
            other => other,
        };
        self.sheets
            .get(&substitute)
            .or_else(|| BUILTIN.sheets.get(&sheet))
            .expect("the built-in skin has every sheet")
    }

    /// The bitmap holding a sprite and the part of it the bitmap covers,
    /// or `None` when the sheet is too small to hold any of it. Winamp drew
    /// whatever was there and nothing where nothing was.
    pub fn sprite(&self, sprite: Sprite) -> Option<(&Bitmap, Sprite)> {
        let bitmap = self.sheet(sprite.sheet);
        let clipped = sprite.clipped_to(bitmap.width, bitmap.height)?;
        Some((bitmap, clipped))
    }
}

/// The largest skin sheet decoded, per side and in decoded bytes. The
/// largest classic sheet is under 500 px a side, and a 32-bit BMP of this
/// size still fits under the archive's 8 MiB entry limit.
const MAX_SHEET_SIDE: u32 = 1024;
// Eight bytes a pixel, so a 16-bit RGBA PNG of the full size still decodes.
const MAX_SHEET_BYTES: u64 = 8 * (MAX_SHEET_SIDE as u64) * (MAX_SHEET_SIDE as u64);

/// Bound traversal of a selected unpacked skin folder.
pub(crate) const MAX_SKIN_DEPTH: usize = 8;

/// Whether a file inside a skin is one this reader looks at, so cursors,
/// readmes, and the equalizer's bitmaps are never inflated.
fn wanted(file_name: &str) -> bool {
    if matches!(
        file_name,
        "pledit.txt" | "viscolor.txt" | "region.txt" | "skin.xml"
    ) {
        return true;
    }
    let Some((stem, extension)) = file_name.rsplit_once('.') else {
        return false;
    };
    matches!(extension, "bmp" | "png") && Sheet::ALL.iter().any(|sheet| sheet.file_stem() == stem)
}

/// The built-in skin, drawn by `examples/default_skin.rs`.
const BUILTIN_ARCHIVE: &[u8] = include_bytes!("../../assets/skins/spotifast.wsz");

static BUILTIN: LazyLock<Arc<Skin>> = LazyLock::new(|| {
    Arc::new(Skin::from_archive("Spotifast", BUILTIN_ARCHIVE).expect("the built-in skin reads"))
});

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-colour PNG of the given size.
    fn png(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb(color));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    fn bmp(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb(color));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Bmp).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn the_built_in_skin_holds_every_sprite_whole() {
        let skin = Skin::builtin();
        for sheet in Sheet::ALL {
            assert!(skin.has(sheet), "{} is missing", sheet.file_stem());
        }
        for (name, sprite) in sprites::ALL {
            let (_, clipped) = skin
                .sprite(*sprite)
                .unwrap_or_else(|| panic!("{name} is off the sheet"));
            assert_eq!(clipped, *sprite, "{name} is cut off");
        }
        for sprite in [
            sprites::volume_frame(27),
            sprites::balance_frame(27),
            sprites::digit(9),
            sprites::digit_ex(9),
            sprites::glyph(2, 30),
        ] {
            assert_eq!(
                skin.sprite(sprite).map(|(_, clipped)| clipped),
                Some(sprite)
            );
        }
        assert!(skin.has_extended_digits());
        assert_eq!(skin.name, "Spotifast");
    }

    #[test]
    fn the_built_in_skin_is_not_blank() {
        let skin = Skin::builtin();
        let (bitmap, sprite) = skin.sprite(sprites::PLAY).unwrap();
        let glyph = bitmap.crop(sprite).unwrap();
        let distinct: std::collections::HashSet<[u8; 4]> = (0..glyph.height)
            .flat_map(|y| (0..glyph.width).map(move |x| (x, y)))
            .filter_map(|(x, y)| glyph.pixel(x, y))
            .collect();
        assert!(distinct.len() >= 3, "the play button is a flat colour");
        let text = skin.sheet(Sheet::Text);
        let a = text.crop(font::glyph('A')).unwrap();
        let blank = text.crop(font::glyph(' ')).unwrap();
        assert_ne!(a, blank);
    }

    #[test]
    fn files_are_found_regardless_of_case_or_folder() {
        let archive = zip::write(&[
            ("Some Skin/", b"", false),
            ("Some Skin/MAIN.BMP", &png(275, 116, [1, 2, 3]), true),
            ("Some Skin/PlEdit.TXT", b"[Text]\nNormal=#123456\n", false),
            ("Some Skin/VISCOLOR.txt", b"9,8,7\n", true),
            ("Some Skin/readme.txt", b"thanks for downloading", false),
        ]);
        let skin = Skin::from_archive("Some Skin", &archive).unwrap();
        assert!(skin.has(Sheet::Main));
        assert_eq!(skin.sheet(Sheet::Main).pixel(0, 0), Some([1, 2, 3, 255]));
        assert_eq!(skin.playlist.normal, [0x12, 0x34, 0x56]);
        assert_eq!(skin.vis_colors[0], [9, 8, 7]);
        assert_eq!(skin.vis_colors[1], config::DEFAULT_VIS_COLORS[1]);
    }

    #[test]
    fn a_skin_saved_with_a_byte_order_mark_is_still_read() {
        const BOM: &[u8] = b"\xef\xbb\xbf";
        let mut pledit = BOM.to_vec();
        pledit.extend_from_slice(b"[Text]\r\nNormal=#123456\r\n");
        let mut viscolor = BOM.to_vec();
        viscolor.extend_from_slice(b"9,8,7\n1,2,3\n");

        let archive = zip::write(&[
            ("main.bmp", &png(275, 116, [1, 2, 3]), true),
            ("pledit.txt", &pledit, false),
            ("viscolor.txt", &viscolor, false),
        ]);
        let skin = Skin::from_archive("bom", &archive).unwrap();

        assert_eq!(skin.playlist.normal, [0x12, 0x34, 0x56]);
        assert_eq!(skin.vis_colors[0], [9, 8, 7]);
        assert_eq!(skin.vis_colors[1], [1, 2, 3]);
    }

    #[test]
    fn the_last_copy_of_a_repeated_file_wins() {
        let archive = zip::write(&[
            ("main.bmp", &png(275, 116, [10, 0, 0]), false),
            ("nested/Main.bmp", &png(275, 116, [0, 20, 0]), false),
        ]);
        let skin = Skin::from_archive("twice", &archive).unwrap();
        assert_eq!(skin.sheet(Sheet::Main).pixel(5, 5), Some([0, 20, 0, 255]));
    }

    #[test]
    fn a_name_repeated_across_the_directory_is_inflated_once() {
        let archive = zip::write(&[
            ("main.bmp", &vec![0u8; 4 << 20], true),
            ("main.bmp", &png(275, 116, [0, 20, 0]), false),
        ]);
        // Point a hundred more directory entries at the large first file.
        let end = archive.len() - 22;
        let directory =
            u32::from_le_bytes(archive[end + 16..end + 20].try_into().unwrap()) as usize;
        let record = &archive[directory..directory + 46 + "main.bmp".len()];
        let mut crafted = archive[..directory].to_vec();
        for _ in 0..100 {
            crafted.extend_from_slice(record);
        }
        crafted.extend_from_slice(&archive[directory..end + 8]);
        let count = (u16::from_le_bytes([archive[end + 8], archive[end + 9]]) + 100).to_le_bytes();
        crafted.extend_from_slice(&count);
        crafted.extend_from_slice(&count);
        let size = (crafted.len() - directory - 12) as u32;
        crafted.extend_from_slice(&size.to_le_bytes());
        crafted.extend_from_slice(&(directory as u32).to_le_bytes());
        crafted.extend_from_slice(&0u16.to_le_bytes());

        let skin = Skin::from_archive("repeated", &crafted).unwrap();
        assert_eq!(skin.sheet(Sheet::Main).pixel(5, 5), Some([0, 20, 0, 255]));
    }

    #[test]
    fn a_name_whose_last_copy_cannot_be_read_is_not_tried_again() {
        let mut archive = zip::write(&[
            ("main.bmp", &png(275, 116, [10, 0, 0]), false),
            ("main.bmp", &png(275, 116, [0, 20, 0]), false),
        ]);
        // Mark the last copy encrypted in the central directory.
        let end = archive.len() - 22;
        let directory =
            u32::from_le_bytes(archive[end + 16..end + 20].try_into().unwrap()) as usize;
        let last = directory + 46 + "main.bmp".len();
        archive[last + 8] |= 1;

        // The earlier copy is not read in its place, so nothing is left.
        assert!(matches!(
            Skin::from_archive("unreadable", &archive),
            Err(SkinError::Empty)
        ));
    }

    #[test]
    fn a_skin_that_inflates_past_the_limit_is_refused() {
        let zeros = vec![0u8; 6 << 20];
        let names = [
            "main", "cbuttons", "titlebar", "posbar", "volume", "balance",
        ];
        let names: Vec<String> = names.iter().map(|stem| format!("{stem}.bmp")).collect();
        let entries: Vec<(&str, &[u8], bool)> = names
            .iter()
            .map(|name| (name.as_str(), zeros.as_slice(), true))
            .collect();
        assert!(matches!(
            Skin::from_archive("huge", &zip::write(&entries)),
            Err(SkinError::Archive(zip::ZipError::TooLarge(_)))
        ));
    }

    #[test]
    fn a_real_bmp_decodes_whatever_it_is_called() {
        let archive = zip::write(&[("cbuttons.png", &bmp(136, 36, [4, 5, 6]), true)]);
        let skin = Skin::from_archive("bmp", &archive).unwrap();
        let sheet = skin.sheet(Sheet::CButtons);
        assert_eq!((sheet.width, sheet.height), (136, 36));
        assert_eq!(sheet.pixel(135, 35), Some([4, 5, 6, 255]));
    }

    #[test]
    fn missing_sheets_come_from_the_built_in_skin_or_a_stand_in() {
        let archive = zip::write(&[
            ("main.bmp", &png(275, 116, [1, 1, 1]), false),
            ("volume.bmp", &png(68, 433, [2, 2, 2]), false),
            ("numbers.bmp", &png(99, 13, [3, 3, 3]), false),
        ]);
        let skin = Skin::from_archive("sparse", &archive).unwrap();
        assert!(!skin.has(Sheet::Balance));
        assert_eq!(skin.sheet(Sheet::Balance).pixel(0, 0), Some([2, 2, 2, 255]));
        assert!(!skin.has_extended_digits());
        assert_eq!(skin.sheet(Sheet::NumsEx).pixel(0, 0), Some([3, 3, 3, 255]));
        assert_eq!(
            skin.sheet(Sheet::CButtons),
            Skin::builtin().sheet(Sheet::CButtons)
        );
        assert_eq!(skin.playlist, PlaylistStyle::default());
    }

    #[test]
    fn a_sheet_too_small_for_a_sprite_gives_what_it_has() {
        let archive = zip::write(&[
            ("posbar.bmp", &png(307, 4, [1, 1, 1]), false),
            ("volume.bmp", &png(68, 419, [2, 2, 2]), false),
        ]);
        let skin = Skin::from_archive("short", &archive).unwrap();
        let (_, track) = skin.sprite(sprites::POSITION_TRACK).unwrap();
        assert_eq!((track.width, track.height), (248, 4));
        assert!(skin.sprite(sprites::VOLUME_THUMB).is_none());
        assert!(skin.sprite(sprites::volume_frame(27)).is_some());
    }

    #[test]
    fn an_unreadable_bitmap_is_skipped_not_fatal() {
        let archive = zip::write(&[
            ("main.bmp", &png(275, 116, [1, 1, 1]), false),
            ("cbuttons.bmp", b"this is not an image", false),
        ]);
        let skin = Skin::from_archive("broken", &archive).unwrap();
        assert!(!skin.has(Sheet::CButtons));
    }

    #[test]
    fn an_oversized_sheet_is_skipped_for_the_built_in_one() {
        let archive = zip::write(&[
            ("main.bmp", &png(275, 116, [1, 1, 1]), false),
            ("cbuttons.bmp", &png(MAX_SHEET_SIDE + 1, 1, [2, 2, 2]), true),
        ]);
        let skin = Skin::from_archive("huge", &archive).unwrap();
        assert!(!skin.has(Sheet::CButtons));
        assert_eq!(
            skin.sheet(Sheet::CButtons),
            Skin::builtin().sheet(Sheet::CButtons)
        );
    }

    #[test]
    fn what_is_not_a_classic_skin_is_named_as_such() {
        assert!(matches!(
            Skin::from_archive("text", b"just some text"),
            Err(SkinError::NotAnArchive)
        ));
        let modern = zip::write(&[("skin.xml", b"<WinampAbstractionLayer/>", false)]);
        assert!(matches!(
            Skin::from_archive("modern", &modern),
            Err(SkinError::ModernSkin)
        ));
        let empty = zip::write(&[("readme.txt", b"nothing here", false)]);
        assert!(matches!(
            Skin::from_archive("empty", &empty),
            Err(SkinError::Empty)
        ));
    }

    #[test]
    fn a_folder_of_bitmaps_is_a_skin_too() {
        let dir = std::env::temp_dir().join(format!("spotifast-skin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MAIN.BMP"), png(275, 116, [7, 7, 7])).unwrap();
        std::fs::write(dir.join("readme.txt"), b"a folder skin").unwrap();
        let skin = Skin::load(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(skin.name, dir.file_name().unwrap().to_string_lossy());
        assert_eq!(skin.sheet(Sheet::Main).pixel(1, 1), Some([7, 7, 7, 255]));
    }

    #[test]
    fn a_folder_skin_is_read_from_the_folder_it_was_unpacked_into() {
        let dir = std::env::temp_dir().join(format!("spotifast-nested-{}", std::process::id()));
        let inner = dir.join("Some Skin");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("MAIN.BMP"), png(275, 116, [9, 9, 9])).unwrap();
        std::fs::write(inner.join("pledit.txt"), b"[Text]\nNormal=#010203\n").unwrap();
        let skin = Skin::load(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(skin.sheet(Sheet::Main).pixel(1, 1), Some([9, 9, 9, 255]));
        assert_eq!(skin.playlist.normal, [1, 2, 3]);
    }

    #[test]
    fn shallower_skin_files_win_across_sibling_subtrees() {
        let dir = std::env::temp_dir().join(format!("spotifast-skin-depth-{}", std::process::id()));
        for folder in ["a/nested", "b", "c"] {
            std::fs::create_dir_all(dir.join(folder)).unwrap();
        }
        std::fs::write(dir.join("a/nested/main.bmp"), png(275, 116, [1, 0, 0])).unwrap();
        std::fs::write(dir.join("b/MAIN.BMP"), png(275, 116, [0, 2, 0])).unwrap();
        std::fs::write(dir.join("c/main.bmp"), png(275, 116, [0, 0, 3])).unwrap();
        let skin = Skin::load(&dir).unwrap();
        assert_eq!(skin.sheet(Sheet::Main).pixel(1, 1), Some([0, 2, 0, 255]));
        std::fs::write(dir.join("main.bmp"), png(275, 116, [4, 4, 4])).unwrap();
        let skin = Skin::load(&dir).unwrap();
        assert_eq!(skin.sheet(Sheet::Main).pixel(1, 1), Some([4, 4, 4, 255]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unpacked_skin_search_stops_at_its_depth_limit() {
        let dir = std::env::temp_dir().join(format!("spotifast-skin-limit-{}", std::process::id()));
        let mut inner = dir.clone();
        for _ in 0..=MAX_SKIN_DEPTH {
            inner = inner.join("nested");
        }
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("main.bmp"), png(275, 116, [1, 2, 3])).unwrap();
        assert!(matches!(Skin::load(&dir), Err(SkinError::Empty)));
        std::fs::write(
            inner.parent().unwrap().join("main.bmp"),
            png(275, 116, [4, 5, 6]),
        )
        .unwrap();
        let skin = Skin::load(&dir).unwrap();
        assert_eq!(skin.sheet(Sheet::Main).pixel(1, 1), Some([4, 5, 6, 255]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unpacked_skin_larger_than_the_limit_is_refused() {
        let dir = std::env::temp_dir().join(format!("spotifast-skin-total-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let large = vec![0u8; SKIN_LIMIT / 4 + 1];
        for stem in ["main", "cbuttons", "titlebar", "posbar"] {
            std::fs::write(dir.join(format!("{stem}.bmp")), &large).unwrap();
        }
        let result = Skin::load(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(
            result,
            Err(SkinError::Archive(zip::ZipError::TooLarge(_)))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unpacked_skin_search_does_not_follow_file_or_directory_links() {
        let dir = std::env::temp_dir().join(format!("spotifast-skin-links-{}", std::process::id()));
        let chosen = dir.join("chosen");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&chosen).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("main.bmp"), png(275, 116, [1, 2, 3])).unwrap();
        std::os::unix::fs::symlink(&outside, chosen.join("linked-folder")).unwrap();
        std::os::unix::fs::symlink(outside.join("main.bmp"), chosen.join("main.bmp")).unwrap();
        assert!(matches!(Skin::load(&chosen), Err(SkinError::Empty)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_is_not_there_is_an_io_error() {
        let missing = Path::new("/nonexistent/skin.wsz");
        assert!(matches!(Skin::load(missing), Err(SkinError::Io(_))));
    }

    /// Loads every skin in `$SPOTIFAST_SKIN_SAMPLES`, when set, to check
    /// the reader against real files without shipping any.
    #[test]
    fn sample_skins_load() {
        let Ok(dir) = std::env::var("SPOTIFAST_SKIN_SAMPLES") else {
            return;
        };
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|extension| extension != "wsz") {
                continue;
            }
            let skin =
                Skin::load(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert!(
                skin.has(Sheet::Main),
                "{} has no main window",
                path.display()
            );
            for (_, sprite) in sprites::ALL {
                let _ = skin.sprite(*sprite);
            }
            seen += 1;
        }
        assert!(seen > 0, "no .wsz files in the samples folder");
    }
}
