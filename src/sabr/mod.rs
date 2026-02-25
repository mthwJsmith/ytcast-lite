pub mod ump;
pub mod stream;

pub mod proto {
    pub mod misc {
        include!(concat!(env!("OUT_DIR"), "/misc.rs"));
    }
    pub mod vs {
        include!(concat!(env!("OUT_DIR"), "/video_streaming.rs"));
    }
}
