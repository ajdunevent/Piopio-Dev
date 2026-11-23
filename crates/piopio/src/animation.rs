//! Animated pictures (GIF, APNG, animated WebP) in a chat: the still
//! thumbnail with a play button; playing decodes the frames on another
//! thread and shows them in turn, and a click pauses.
//!
//! Frames are decoded here rather than by the system's image loaders, so
//! every format moves the same everywhere.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use adw::{gdk, glib, gtk};
use image::AnimationDecoder;

/// Frames kept in memory at most (RGBA bytes), so a huge GIF can't eat it.
const MAX_BYTES: usize = 256 * 1024 * 1024;

struct Frame {
    texture: gdk::MemoryTexture,
    delay: Duration,
}

/// A decoded frame: width, height, RGBA pixels, and how long it shows.
type RawFrame = (u32, u32, Vec<u8>, Duration);

fn decode(path: &str) -> anyhow::Result<Vec<RawFrame>> {
    let data = std::fs::read(path)?;
    let cursor = std::io::Cursor::new(&data);
    let frames: Box<dyn Iterator<Item = image::ImageResult<image::Frame>>> =
        match image::guess_format(&data)? {
            image::ImageFormat::Gif => {
                Box::new(image::codecs::gif::GifDecoder::new(cursor)?.into_frames())
            }
            image::ImageFormat::Png => Box::new(
                image::codecs::png::PngDecoder::new(cursor)?
                    .apng()?
                    .into_frames(),
            ),
            image::ImageFormat::WebP => {
                Box::new(image::codecs::webp::WebPDecoder::new(cursor)?.into_frames())
            }
            other => anyhow::bail!("{other:?} doesn't animate"),
        };
    let mut out = Vec::new();
    let mut total = 0;
    for frame in frames {
        let frame = frame?;
        let (num, den) = frame.delay().numer_denom_ms();
        // Browsers treat very short delays as 100 ms; so do we.
        let ms = num.checked_div(den).unwrap_or(100);
        let delay = Duration::from_millis(if ms < 20 { 100 } else { ms as u64 });
        let buffer = frame.into_buffer();
        let (w, h) = buffer.dimensions();
        total += buffer.as_raw().len();
        if total > MAX_BYTES {
            break;
        }
        out.push((w, h, buffer.into_raw(), delay));
    }
    anyhow::ensure!(!out.is_empty(), "no frames");
    Ok(out)
}

/// The thumbnail with a play button over it; `open` runs on a click while
/// it isn't playing.
pub fn player(thumbnail: &str, path: &str, open: impl Fn() + 'static) -> gtk::Widget {
    let picture = gtk::Picture::for_filename(thumbnail);
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_size_request(240, 180);
    picture.add_css_class("attachment-picture");
    let play = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text("Play")
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["osd", "circular", "large-button"])
        .build();
    let overlay = gtk::Overlay::builder().child(&picture).build();
    overlay.add_overlay(&play);

    let frames: Rc<RefCell<Option<Rc<Vec<Frame>>>>> = Rc::default();
    // Bumped on every play and pause, so an old timer stops by itself.
    let generation = Rc::new(Cell::new(0u64));
    let playing = Rc::new(Cell::new(false));

    let run = {
        let (picture, generation) = (picture.clone(), generation.clone());
        move |frames: Rc<Vec<Frame>>| {
            let mine = generation.get();
            let index = Rc::new(Cell::new(0usize));
            fn step(
                picture: gtk::Picture,
                frames: Rc<Vec<Frame>>,
                index: Rc<Cell<usize>>,
                generation: Rc<Cell<u64>>,
                mine: u64,
            ) {
                if generation.get() != mine || picture.root().is_none() {
                    return;
                }
                let frame = &frames[index.get() % frames.len()];
                picture.set_paintable(Some(&frame.texture));
                index.set(index.get() + 1);
                let delay = frame.delay;
                glib::timeout_add_local_once(delay, move || {
                    step(picture, frames, index, generation, mine)
                });
            }
            step(picture.clone(), frames, index, generation.clone(), mine);
        }
    };

    {
        let (path, frames, playing, generation, play_button) = (
            path.to_owned(),
            frames.clone(),
            playing.clone(),
            generation.clone(),
            play.clone(),
        );
        play.connect_clicked(move |b| {
            playing.set(true);
            generation.set(generation.get() + 1);
            b.set_visible(false);
            if let Some(f) = frames.borrow().clone() {
                run(f);
                return;
            }
            // Decode once, off the main thread.
            let (path, frames, run, play_button) = (
                path.clone(),
                frames.clone(),
                run.clone(),
                play_button.clone(),
            );
            glib::spawn_future_local(async move {
                let decoded = gtk::gio::spawn_blocking(move || decode(&path)).await;
                match decoded {
                    Ok(Ok(raw)) => {
                        let list: Vec<Frame> = raw
                            .into_iter()
                            .map(|(w, h, rgba, delay)| Frame {
                                texture: gdk::MemoryTexture::new(
                                    w as i32,
                                    h as i32,
                                    gdk::MemoryFormat::R8g8b8a8,
                                    &glib::Bytes::from_owned(rgba),
                                    w as usize * 4,
                                ),
                                delay,
                            })
                            .collect();
                        let list = Rc::new(list);
                        *frames.borrow_mut() = Some(list.clone());
                        run(list);
                    }
                    _ => {
                        play_button.set_visible(true);
                        play_button.set_sensitive(false);
                        play_button.set_tooltip_text(Some("Can’t play this picture"));
                    }
                }
            });
        });
    }

    // A click pauses while playing, and opens the file otherwise.
    let click = gtk::GestureClick::new();
    {
        let (playing, generation, play) = (playing.clone(), generation.clone(), play.clone());
        click.connect_released(move |_, _, _, _| {
            if playing.get() {
                playing.set(false);
                generation.set(generation.get() + 1);
                play.set_visible(true);
            } else {
                open();
            }
        });
    }
    picture.add_controller(click);
    overlay.upcast()
}
