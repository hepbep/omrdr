# omrdr (osu! mania renderer)

Just a little app to turn osu! mania replays into mp4 video running on your own computer.

All you need to do is to drag and drop in an osu! replay file (.osr). The map file is downloaded automatically. 
The app renders a playfield with combo, accuracy, unstable rate bar, and judgement counts along with the actual replay.

You can currently change settings for things such as:
- resolution
- fps
- scroll speed (accurate to osu!'s 1 to 40 scale)
- slider velocity (sv) changes
- column width
- hit line height
- music volume
- video quality

Replays uploaded, beatmaps downloaded, and videos generated are kept in the "replays/", "beatmaps/", and "output/" in the app's directory.

Generated videos are automatically named after the replay, but can be set to a custom title. 

## How to install
1. Download the latest release from the [Releases page](https://github.com/hepbep/omrdr/releases)
2. Extract the .zip file anywhere and run the **"omrdr.exe"** file.
3. Change the settings to your suit your desires.
4. #click dat **Render video** button. The video will be saved in the "output/" folder in the app's directory.
                    <img width="567" height="99" alt="image" src="https://github.com/user-attachments/assets/a2beb781-c660-4ef3-a648-bd227fd51b0f" />

**OR**

## Build from source
Download [Rust](https://rustup.rs). On windows you will also need the Visual Studio C++ build tools. You need to have ffmpeg on ur PATH or next to the built .exe to render.
```
git clone https://github.com/hepbep/omrdr.git
cd omrdr
cargo run --release
```

## Disclaimer 
This is not affiliated at all or endorsed by osu! or ppy. Beatmaps are downloaded from community mirrors and all beatmaps and music belong to their creators.
