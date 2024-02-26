use std::path::{Path, PathBuf};
use std::collections::HashSet;
use std::io::{self, BufReader, BufRead, ErrorKind, ErrorKind::WouldBlock};
use std::fs::File;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::ops::{Deref, DerefMut};
use std::process::Stdio;
use std::time::{Instant};
use tokio::sync:: {Mutex};
use tokio::task::JoinHandle;
use tokio::time::{Duration};
use tokio::io::{AsyncWriteExt};
use serde::{Serialize, Deserialize};
use tauri::State;
use tokio::process::{Command, ChildStderr, ChildStdin};
use tokio::join;
use futures::future::join_all;

use crate::utils::{ffmpeg_path_as_str, monitor_and_log_recording_start};
use crate::upload::upload_file;

use crate::audio::AudioRecorder;

const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000 / 30);

pub struct RecordingState {
  pub screen_process: Option<tokio::process::Child>,
  pub screen_process_stdin: Option<Arc<Mutex<ChildStdin>>>,
  pub video_process: Option<tokio::process::Child>,
  pub audio_process: Option<AudioRecorder>,
  pub upload_handles: Mutex<Vec<JoinHandle<Result<(), String>>>>,
  pub recording_options: Option<RecordingOptions>,
  pub shutdown_flag: Arc<AtomicBool>,
  pub video_uploading_finished: Arc<AtomicBool>,
  pub audio_uploading_finished: Arc<AtomicBool>,
  pub data_dir: Option<PathBuf>
}

unsafe impl Send for RecordingState {}
unsafe impl Sync for RecordingState {}
unsafe impl Send for AudioRecorder {}
unsafe impl Sync for AudioRecorder {}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RecordingOptions {
  pub user_id: String,
  pub video_id: String,
  pub screen_index: String,
  pub video_index: String,
  pub audio_name: String,
  pub aws_region: String,
  pub aws_bucket: String,
  pub framerate: String,
  pub resolution: String,
}

#[tauri::command]
pub async fn start_dual_recording(
  state: State<'_, Arc<Mutex<RecordingState>>>,
  options: RecordingOptions,
) -> Result<(), String> {
  println!("Starting screen recording...");
  let mut state_guard = state.lock().await;
  
  let shutdown_flag = Arc::new(AtomicBool::new(false));

  // let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
  // let display = Display::primary().map_err(|_| "Failed to find primary display".to_string())?;
  // let (w, h) = (display.width(), display.height());
  // let adjusted_height = h & !1;
  // let capture_size = w * adjusted_height * 4;

  let data_dir = state_guard.data_dir.as_ref()
      .ok_or("Data directory is not set in the recording state".to_string())?.clone();

  println!("data_dir: {:?}", data_dir);
  
  let audio_chunks_dir = data_dir.join("chunks/audio");
  let video_chunks_dir = data_dir.join("chunks/video");
  clean_and_create_dir(&audio_chunks_dir)?;
  clean_and_create_dir(&video_chunks_dir)?;
  
  let audio_name = if options.audio_name.is_empty() {
    None
  } else {
    Some(options.audio_name.clone())
  };
  
  // let ffmpeg_screen_args_future = construct_recording_args(&options, &screen_chunks_dir, "screen", &options.screen_index, w, adjusted_height);
  // let ffmpeg_screen_args = ffmpeg_screen_args_future.await.map_err(|e| e.to_string())?;

  let screenshot_output_path = data_dir.join("screen-capture.jpg").to_str().unwrap().to_string();
  let ffmpeg_screen_screenshot_args = match std::env::consts::OS {
    "macos" => vec![
        "-y".to_string(),
        "-f".to_string(), 
        "avfoundation".to_string(), 
        "-i".to_string(), 
        options.screen_index.clone(), 
        "-vframes".to_string(), 
        "1".to_string(), 
        screenshot_output_path.clone()
    ],
    "windows" => vec![
        "-y".to_string(),
        "-f".to_string(),
        "gdigrab".to_string(), 
        "-i".to_string(), 
        "desktop".to_string(), 
        "-vframes".to_string(), 
        "1".to_string(), 
        screenshot_output_path.clone()
    ],
    "linux" => vec![
        "-y".to_string(),
        "-f".to_string(), 
        "x11grab".to_string(), 
        "-i".to_string(), 
        ":0.0".to_string(), 
        "-vframes".to_string(), 
        "1".to_string(), 
        screenshot_output_path.clone()
    ],
    _ => return Err("Unsupported OS".to_string()),
  };
  
  // println!("Screen args: {:?}", ffmpeg_screen_args);

  // Prepare screen and audio recording concurrently
  let ffmpeg_binary_path_str = ffmpeg_path_as_str()?;
  // let screen_recording_preparation = prepare_screen_recording(&ffmpeg_binary_path_str, &options, &screen_chunks_dir, w, adjusted_height);
  let audio_recording_preparation = prepare_audio_recording(&options, &audio_chunks_dir, &video_chunks_dir, audio_name);

  let audio_recording_result = audio_recording_preparation.await.map_err(|e| e.to_string())?;

  // Handle the results of both operations
  // let (screen_child, screen_stdin) = screen_recording_result.map_err(|e| e.to_string())?;

  // let screen_stdin_arc = Arc::new(Mutex::new(screen_stdin));
  // let stdin_clone = Arc::clone(&screen_stdin_arc);

  // tokio::spawn(async move {
  //     while let Some(frame_data) = rx.recv().await {
  //         let mut stdin_locked = stdin_clone.lock().await;
  //         if let Err(e) = stdin_locked.write_all(&frame_data).await {
  //             eprintln!("Failed to write frame data to FFmpeg's stdin: {}", e);
  //             break;
  //         }
  //     }
  // });

  let options_clone = state_guard.recording_options.clone();  

  // Spawn the screenshot task without directly awaiting it
  tokio::spawn(async move {
      if let Err(e) = take_screenshot(
          ffmpeg_binary_path_str.clone(),
          ffmpeg_screen_screenshot_args.clone(),
          screenshot_output_path.clone(),
          options_clone.clone(),
      ).await {
          eprintln!("Failed to take and upload screenshot: {}", e);
      }
  });

  // state_guard.screen_process = Some(screen_child);
  // println!("Set screen child");
  // state_guard.screen_process_stdin = Some(screen_stdin_arc);
  // println!("Set screen stdin");
  state_guard.audio_process = Some(audio_recording_result);
  state_guard.upload_handles = Mutex::new(vec![]);
  state_guard.recording_options = Some(options.clone());
  state_guard.shutdown_flag = shutdown_flag.clone();
  state_guard.video_uploading_finished = Arc::new(AtomicBool::new(false));
  state_guard.audio_uploading_finished = Arc::new(AtomicBool::new(false));

  // let _capturer_thread = {
  //     println!("Starting video capture thread...");
  //     let shutdown_flag = shutdown_flag.clone();
  //     tokio::spawn(async move {
  //         println!("Starting video capture...");

  //         let mut safe_capturer = SafeCapturer(Capturer::new(display).expect("Failed to start capture"));
  //         let mut next_frame_time = Instant::now();
  //         while !shutdown_flag.load(Ordering::SeqCst) {
  //             let now = Instant::now();
  //             if now < next_frame_time {
  //                 std::thread::sleep(next_frame_time - now);
  //             }
  //             next_frame_time = Instant::now() + FRAME_INTERVAL;

  //             let frame_data = match safe_capturer.frame() {
  //                 Ok(frame) => {
  //                     let stride = frame[..capture_size].len() / adjusted_height;
  //                     let mut frame_data = Vec::with_capacity(capture_size);
  //                     for row in 0..adjusted_height {
  //                         let start = row * stride;
  //                         let end = start + stride;
  //                         frame_data.extend_from_slice(&frame[start..end]);
  //                     }
  //                     Some(frame_data)
  //                 },
  //                 Err(error) if error.kind() == WouldBlock => {
  //                     continue;
  //                 },
  //                 Err(error) => return Err(format!("Capture error: {}", error)),
  //             };

  //             if let Some(frame_data) = frame_data {
  //                 if let Err(e) = tx.send(frame_data).await {
  //                     eprintln!("Failed to send frame data through channel: {}", e);
  //                     break;
  //                 }
  //             }
  //         }

  //         Ok(())
  //     })
  // };

  let screen_upload = start_upload_loop(video_chunks_dir.clone(), options.clone(), "video".to_string(), shutdown_flag.clone(), state_guard.video_uploading_finished.clone());
  let audio_upload = start_upload_loop(audio_chunks_dir, options.clone(), "audio".to_string(), shutdown_flag.clone(), state_guard.audio_uploading_finished.clone());

  drop(state_guard);

  println!("Starting upload loops...");


  match tokio::try_join!(screen_upload, audio_upload) {
      Ok(_) => {
          println!("Both upload loops completed successfully.");
      },
      Err(e) => {
          eprintln!("An error occurred: {}", e);
      },
  }

  Ok(())
}

#[tauri::command]
pub async fn stop_all_recordings(state: State<'_, Arc<Mutex<RecordingState>>>) -> Result<(), String> {
    println!("!!STOPPING screen recording...");

    let mut guard = state.lock().await;
    
    println!("Stopping screen recording...");
    
    guard.shutdown_flag.store(true, Ordering::SeqCst);

    // if let Some(stdin) = guard.screen_process_stdin.take() {
    //     println!("Sending quit command to FFmpeg...");
    //     let mut stdin_locked = stdin.lock().await;
    //     if let Err(e) = graceful_stop_ffmpeg(&mut stdin_locked).await {
    //         eprintln!("Failed to send quit command to FFmpeg: {}", e);
    //     }
    // }

    if let Some(mut audio_process) = guard.audio_process.take() {
        println!("Stopping audio recording...");
        audio_process.stop_audio_recording().await.expect("Failed to stop audio recording");
    }

    while !guard.video_uploading_finished.load(Ordering::SeqCst) 
        || !guard.audio_uploading_finished.load(Ordering::SeqCst) {
        println!("Waiting for uploads to finish...");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    
    println!("All recordings and uploads stopped.");

    Ok(())
}

fn clean_and_create_dir(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        // Instead of just reading the directory, this will also handle subdirectories.
        std::fs::remove_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    let segment_list_path = dir.join("segment_list.txt");
    match File::open(&segment_list_path) {
        Ok(_) => Ok(()),
        Err(ref e) if e.kind() == ErrorKind::NotFound => {
            File::create(&segment_list_path).map_err(|e| e.to_string())?;
            Ok(())
        },
        Err(e) => Err(e.to_string()), 
    }
}

// async fn construct_recording_args(
//     options: &RecordingOptions,
//     chunks_dir: &Path, 
//     video_type: &str,
//     input_index: &str, 
//     w: usize,
//     adjusted_height: usize,
// ) -> Result<Vec<String>, String> {
//     let output_filename_pattern = format!("{}/recording_chunk_%03d.ts", chunks_dir.display());
//     let segment_list_filename = format!("{}/segment_list.txt", chunks_dir.display());
    
//     ensure_segment_list_exists(PathBuf::from(&segment_list_filename))
//         .map_err(|e| format!("Failed to ensure segment list file exists: {}", e))?;
      
//     let fps = if video_type == "screen" { "30" } else { &options.framerate };
//     let preset = "veryfast".to_string();
//     let crf = "28".to_string();
//     let pix_fmt = "yuv420p".to_string();
//     let codec = "libx264".to_string();
//     let segment_time = "3".to_string();
//     let segment_list_type = "flat".to_string();

//     match std::env::consts::OS {
//         "macos" => {
//             Ok(vec![
//                 "-f".to_string(), "rawvideo".to_string(),
//                 "-pix_fmt".to_string(), "bgra".to_string(),
//                 "-s".to_string(), format!("{}x{}", w, adjusted_height),
//                 "-r".to_string(), "30".to_string(),
//                 "-thread_queue_size".to_string(), "8192".to_string(),
//                 "-i".to_string(), "pipe:0".to_string(),
//                 "-c:v".to_string(), "libx264".to_string(),
//                 "-preset".to_string(), preset,
//                 "-pix_fmt".to_string(), pix_fmt,
//                 "-vsync".to_string(), "1".to_string(),
//                 "-r".to_string(), "30".to_string(),
//                 "-an".to_string(),
//                 "-f".to_string(), "segment".to_string(),
//                 "-segment_time".to_string(), segment_time,
//                 "-segment_format".to_string(), "mpegts".to_string(),
//                 "-segment_list".to_string(), segment_list_filename,
//                 "-segment_list_type".to_string(), segment_list_type,
//                 "-reset_timestamps".to_string(), "1".to_string(),
//                 "-use_wallclock_as_timestamps".to_string(), "1".to_string(),
//                 output_filename_pattern,    
//             ])
//         },
//         "linux" => {
//             Ok(vec![
//                 "-f".to_string(), "x11grab".to_string(),
//                 "-i".to_string(), format!("{}+0,0", input_index),
//                 "-draw_mouse".to_string(), "1".to_string(),
//                 "-pix_fmt".to_string(), pix_fmt,
//                 "-c:v".to_string(), codec,
//                 "-crf".to_string(), crf,
//                 "-preset".to_string(), preset,
//                 "-r".to_string(), fps.to_string(),
//                 "-an".to_string(),
//                 "-f".to_string(), "segment".to_string(),
//                 "-segment_time".to_string(), segment_time,
//                 "-segment_format".to_string(), "mpegts".to_string(),
//                 "-segment_list".to_string(), segment_list_filename,
//                 "-segment_list_type".to_string(), segment_list_type,
//                 "-reset_timestamps".to_string(), "1".to_string(),
//                 output_filename_pattern,
//             ])
//         },
//         "windows" => {
//             Ok(vec![
//                 "-f".to_string(), "gdigrab".to_string(),
//                 "-i".to_string(), "desktop".to_string(),
//                 "-pixel_format".to_string(), pix_fmt,
//                 "-c:v".to_string(), codec,
//                 "-crf".to_string(), crf,
//                 "-preset".to_string(), preset,
//                 "-r".to_string(), fps.to_string(),
//                 "-an".to_string(), // This is the argument to skip audio recording.
//                 "-f".to_string(), "segment".to_string(),
//                 "-segment_time".to_string(), segment_time,
//                 "-segment_format".to_string(), "mpegts".to_string(),
//                 "-segment_list".to_string(), segment_list_filename,
//                 "-segment_list_type".to_string(), segment_list_type,
//                 "-reset_timestamps".to_string(), "1".to_string(),
//                 output_filename_pattern,
//             ])
//         },
//         _ => Err("Unsupported OS".to_string()),
//     }
// }

async fn start_upload_loop(
    chunks_dir: PathBuf,
    options: RecordingOptions,
    video_type: String,
    shutdown_flag: Arc<AtomicBool>,
    uploading_finished: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut watched_segments: HashSet<String> = HashSet::new();
    let mut is_final_loop = false;
    loop {
        let mut upload_tasks = vec![];
        if shutdown_flag.load(Ordering::SeqCst) {
            if is_final_loop {
                break;
            }
            is_final_loop = true;
        }

        let current_segments = load_segment_list(&chunks_dir.join("segment_list.txt"))
            .map_err(|e| e.to_string())?
            .difference(&watched_segments)
            .cloned()
            .collect::<HashSet<String>>();

        for segment_filename in &current_segments {
            let segment_path = chunks_dir.join(segment_filename);
            if segment_path.is_file() {
                let options_clone = options.clone();
                let video_type_clone = video_type.clone();
                let segment_path_clone = segment_path.clone();
                // Create a task for each file to be uploaded
                upload_tasks.push(tokio::spawn(async move {
                    let filepath_str = segment_path_clone.to_str().unwrap_or_default().to_owned();
                    println!("Uploading video for {}: {}", video_type_clone, filepath_str);
                    upload_file(Some(options_clone), filepath_str, video_type_clone).await.map(|_| ())
                }));
            }
            watched_segments.insert(segment_filename.clone());
        }

        // Await all initiated upload tasks in parallel
        if !upload_tasks.is_empty() {
            let _ = join_all(upload_tasks).await;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    uploading_finished.store(true, Ordering::SeqCst);
    Ok(())
}

fn ensure_segment_list_exists(file_path: PathBuf) -> io::Result<()> {
    match File::open(&file_path) {
        Ok(_) => (), 
        Err(ref e) if e.kind() == ErrorKind::NotFound => {
            File::create(&file_path)?;
        },
        Err(e) => {
            return Err(e);
        },
    }
    Ok(())
}

fn load_segment_list(segment_list_path: &Path) -> io::Result<HashSet<String>> {
    let file = File::open(segment_list_path)?;
    let reader = BufReader::new(file);

    let mut segments = HashSet::new();
    for line_result in reader.lines() {
        let line = line_result?;
        if !line.is_empty() {
            segments.insert(line);
        }
    }

    Ok(segments)
}

async fn take_screenshot(
    ffmpeg_binary_path_str: String, 
    ffmpeg_screen_screenshot_args: Vec<String>,
    screenshot_path: String,
    options: Option<RecordingOptions>,
) -> Result<(), String> {
    println!("Waiting for 3 seconds before taking the screenshot...");
    tokio::time::sleep(Duration::from_secs(3)).await;
    
    // Start the FFmpeg process for taking a screenshot
    let mut screen_screenshot_child = tokio::process::Command::new(&ffmpeg_binary_path_str)
        .args(&ffmpeg_screen_screenshot_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    match screen_screenshot_child.wait().await {
        Ok(status) if status.success() => {
            println!("Screenshot captured: {}", &screenshot_path);
            if let Some(ref opts) = options {
                match upload_file(Some(opts.clone()), screenshot_path.clone(), "screenshot".to_string()).await {
                    Ok(_) => println!("Screenshot uploaded successfully."),
                    Err(e) => eprintln!("Failed to upload screenshot: {}", e),
                }
            } else {
                eprintln!("No recording options set, skipping upload.");
            }
        },
        Ok(_) => eprintln!("Screenshot command completed, but may have failed."),
        Err(e) => eprintln!("Failed to execute screenshot command: {}", e),
    }

    Ok(())
}

async fn upload_jpeg_files(
    dir_path: &PathBuf,
    options: Option<RecordingOptions>,
) -> Result<(), String> {
    let dir_entries = std::fs::read_dir(dir_path).map_err(|e| format!("Failed to read dir: {}", e))?;
    for entry in dir_entries {
        let entry = entry.map_err(|e| format!("Failed to process dir entry: {}", e))?;
        let path = entry.path();
        if path.is_file() && path.extension().map_or(false, |ext| ext == "jpeg" || ext == "jpg") {
            let file_path_str = path.to_str().unwrap_or_default();
            println!("Found JPEG file for upload: {}", file_path_str);
            upload_file(options.clone(), file_path_str.to_string(), "screenshot".to_string()).await.map_err(|e| format!("Failed to upload JPEG: {}", e))?;
        }
    }

    Ok(())
}

// async fn prepare_screen_recording(
//   ffmpeg_binary_path_str: &str,
//   options: &RecordingOptions,
//   screen_chunks_dir: &Path,
//   w: usize,
//   adjusted_height: usize,
// ) -> Result<(tokio::process::Child, ChildStdin), String> {
//   let ffmpeg_screen_args = construct_recording_args(options, screen_chunks_dir, "screen", &options.screen_index, w, adjusted_height).await.map_err(|e| e.to_string())?;
//   let (screen_child, screen_stderr, screen_stdin) = start_screen_recording_process(ffmpeg_binary_path_str, &ffmpeg_screen_args).await.map_err(|e| e.to_string())?;
  
//   let video_id = options.video_id.clone();
//   tokio::spawn(async move {
//     if let Err(e) = monitor_and_log_recording_start(screen_stderr, &video_id, "video").await {
//       eprintln!("Error monitoring screen recording start: {}", e);
//     }
//   });

//   Ok((screen_child, screen_stdin))
// }

async fn prepare_audio_recording(
  options: &RecordingOptions,
  audio_chunks_dir: &Path,
  video_chunks_dir: &Path,
  audio_name: Option<String>,
) -> Result<AudioRecorder, String> {
  // Assuming `AudioRecorder::start_audio_recording` is an async function.
  // Prepare your AudioRecorder and start recording
  let mut audio_recorder = AudioRecorder::new();
  let audio_file_path = audio_chunks_dir.to_str().unwrap();
  let video_file_path = video_chunks_dir.to_str().unwrap();
  audio_recorder.start_audio_recording(options.clone(), audio_file_path, video_file_path, audio_name.as_ref().map(String::as_str)).await?;
  Ok(audio_recorder)
}

// async fn start_screen_recording_process(ffmpeg_binary_path_str: &str, ffmpeg_screen_args: &[String]) -> Result<(tokio::process::Child, ChildStderr, ChildStdin), io::Error> {
//     let mut child = Command::new(ffmpeg_binary_path_str)
//         .args(ffmpeg_screen_args)
//         .stdin(Stdio::piped())
//         .stdout(Stdio::piped())
//         .stderr(Stdio::piped())
//         .spawn()?;
    
//     let stderr = child.stderr.take().expect("failed to take child stdout");
//     let stdin = child.stdin.take().expect("failed to take child stdin");
    
//     Ok((child, stderr, stdin))
// }

// async fn graceful_stop_ffmpeg(stdin: &mut tokio::process::ChildStdin) -> Result<(), std::io::Error> {
//     stdin.write_all(b"q\n").await?;
//     Ok(())
// }