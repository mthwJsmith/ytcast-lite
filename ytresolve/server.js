import http from 'node:http';
import { SabrStream } from 'googlevideo/sabr-stream';
import { EnabledTrackTypes } from 'googlevideo/utils';

const PORT = parseInt(process.env.PORT || '3033', 10);
const SECRET = process.env.YTRESOLVE_SECRET || '';

// InnerTube client configs (raw calls — no YouTube.js, no SABR capability advertised)
const IOS_CLIENT = {
  clientName: 'IOS', clientVersion: '19.45.4',
  deviceMake: 'Apple', deviceModel: 'iPhone16,2',
  osName: 'iPhone', osVersion: '18.1.0.22B83', hl: 'en', gl: 'US',
};
const IOS_UA = 'com.google.ios.youtube/19.45.4 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)';

const ANDROID_CLIENT = {
  clientName: 'ANDROID', clientVersion: '19.44.38',
  osName: 'Android', osVersion: '14', androidSdkVersion: 34,
  deviceMake: 'Google', deviceModel: 'Pixel 8', hl: 'en', gl: 'US',
};
const ANDROID_UA = 'com.google.android.youtube/19.44.38 (Linux; U; Android 14; en_US; Pixel 8) gzip';

const CLIENTS = [
  { name: 'IOS', client: IOS_CLIENT, ua: IOS_UA, id: 5 },
  { name: 'ANDROID', client: ANDROID_CLIENT, ua: ANDROID_UA, id: 3 },
];

// Raw InnerTube player call
async function innertubePlayer(videoId, client, ua) {
  const resp = await fetch('https://www.youtube.com/youtubei/v1/player?prettyPrint=false', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'User-Agent': ua },
    body: JSON.stringify({
      context: { client },
      videoId,
      contentCheckOk: true,
      racyCheckOk: true,
    }),
  });
  return resp.json();
}

// Try to get a direct audio URL (fast path)
async function resolveDirectUrl(videoId) {
  for (const { name, client, ua } of CLIENTS) {
    try {
      const data = await innertubePlayer(videoId, client, ua);
      if (data.playabilityStatus?.status !== 'OK') continue;

      const formats = data.streamingData?.adaptiveFormats || [];
      const audio = formats
        .filter(f => f.mimeType?.startsWith('audio/') && f.url)
        .sort((a, b) => (b.bitrate || 0) - (a.bitrate || 0));

      if (audio.length > 0) {
        return {
          url: audio[0].url,
          title: data.videoDetails?.title || videoId,
          duration: parseInt(data.videoDetails?.lengthSeconds || '0'),
          client: name,
        };
      }
    } catch (e) {
      console.log(`[resolve] ${name} failed: ${e.message}`);
    }
  }
  return null;
}

// Get SABR streaming data for a video
async function getSabrData(videoId) {
  for (const { name, client, ua, id } of CLIENTS) {
    try {
      const data = await innertubePlayer(videoId, client, ua);
      if (data.playabilityStatus?.status !== 'OK') continue;

      const sd = data.streamingData;
      if (!sd?.serverAbrStreamingUrl) continue;

      const formats = (sd.adaptiveFormats || []).map(f => ({
        itag: f.itag, lastModified: f.lastModified || '0',
        mimeType: f.mimeType, audioQuality: f.audioQuality,
        bitrate: f.bitrate, averageBitrate: f.averageBitrate,
        quality: f.quality, qualityLabel: f.qualityLabel,
        approxDurationMs: parseInt(f.approxDurationMs || '0'),
        contentLength: parseInt(f.contentLength || '0'),
        width: f.width, height: f.height,
        audioTrackId: f.audioTrack?.id,
      }));

      return {
        sabrUrl: sd.serverAbrStreamingUrl,
        ustreamerConfig: data.playerConfig?.mediaCommonConfig
          ?.mediaUstreamerRequestConfig?.videoPlaybackUstreamerConfig,
        clientInfo: {
          osName: client.osName, osVersion: client.osVersion,
          clientName: id, clientVersion: client.clientVersion,
        },
        formats,
        durationMs: parseInt(data.videoDetails?.lengthSeconds || '0') * 1000,
        title: data.videoDetails?.title || videoId,
        duration: parseInt(data.videoDetails?.lengthSeconds || '0'),
        client: name,
      };
    } catch (e) {
      console.log(`[sabr-data] ${name} failed: ${e.message}`);
    }
  }
  return null;
}

// Active SABR streams (so we can abort on client disconnect)
const activeStreams = new Map();

const server = http.createServer(async (req, res) => {
  // Auth check
  if (SECRET && req.headers['x-ytresolve-secret'] !== SECRET) {
    res.writeHead(403, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: 'forbidden' }));
    return;
  }

  // GET /resolve/:videoId — returns direct URL (fast, for MPD)
  const resolveMatch = req.url?.match(/^\/resolve\/([a-zA-Z0-9_-]{11})$/);
  if (resolveMatch) {
    const videoId = resolveMatch[1];
    const start = Date.now();
    try {
      const result = await resolveDirectUrl(videoId);
      if (result) {
        console.log(`[resolve] ${videoId} → ${result.title} via ${result.client} (${Date.now() - start}ms)`);
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify(result));
      } else {
        // No direct URL — tell client to use /stream/ instead
        console.log(`[resolve] ${videoId} → no direct URL, use /stream/ (${Date.now() - start}ms)`);
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ error: 'no_direct_url', use_stream: true }));
      }
    } catch (err) {
      console.error(`[resolve] ${videoId} FAILED (${Date.now() - start}ms):`, err.message);
      res.writeHead(500, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: err.message }));
    }
    return;
  }

  // GET /stream/:videoId — SABR audio proxy stream
  const streamMatch = req.url?.match(/^\/stream\/([a-zA-Z0-9_-]{11})$/);
  if (streamMatch) {
    const videoId = streamMatch[1];
    const start = Date.now();

    try {
      console.log(`[stream] ${videoId} starting SABR...`);
      const sabrData = await getSabrData(videoId);
      if (!sabrData) {
        res.writeHead(404, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ error: 'video not found or no SABR data' }));
        return;
      }

      const sabrStream = new SabrStream({
        serverAbrStreamingUrl: sabrData.sabrUrl,
        videoPlaybackUstreamerConfig: sabrData.ustreamerConfig,
        clientInfo: sabrData.clientInfo,
        formats: sabrData.formats,
        durationMs: sabrData.durationMs,
      });

      const { audioStream, selectedFormats } = await sabrStream.start({
        enabledTrackTypes: EnabledTrackTypes.AUDIO_ONLY,
        preferOpus: true,
        maxRetries: 5,
      });

      const mime = selectedFormats.audioFormat?.mimeType || 'audio/webm; codecs="opus"';
      console.log(`[stream] ${videoId} → ${sabrData.title} via ${sabrData.client} SABR (${mime}) (${Date.now() - start}ms)`);

      // Track active stream
      activeStreams.set(videoId, sabrStream);

      res.writeHead(200, {
        'Content-Type': mime.split(';')[0].trim(),
        'Transfer-Encoding': 'chunked',
        'X-Title': encodeURIComponent(sabrData.title),
        'X-Duration': String(sabrData.duration),
      });

      // Pipe SABR audio stream to HTTP response
      const reader = audioStream.getReader();
      const pump = async () => {
        try {
          while (true) {
            const { done, value } = await reader.read();
            if (done) break;
            if (res.destroyed) break;
            const canContinue = res.write(value);
            if (!canContinue) {
              await new Promise(resolve => res.once('drain', resolve));
            }
          }
          res.end();
        } catch (err) {
          if (!res.destroyed) {
            console.error(`[stream] ${videoId} error:`, err.message);
            res.destroy();
          }
        } finally {
          activeStreams.delete(videoId);
        }
      };

      // Abort SABR if client disconnects
      req.on('close', () => {
        console.log(`[stream] ${videoId} client disconnected`);
        sabrStream.abort();
        activeStreams.delete(videoId);
      });

      pump();
    } catch (err) {
      console.error(`[stream] ${videoId} FAILED (${Date.now() - start}ms):`, err.message);
      if (!res.headersSent) {
        res.writeHead(500, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ error: err.message }));
      }
    }
    return;
  }

  // GET /health
  if (req.url === '/health') {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ ok: true, active_streams: activeStreams.size }));
    return;
  }

  res.writeHead(404, { 'Content-Type': 'application/json' });
  res.end(JSON.stringify({
    error: 'not found',
    endpoints: {
      '/resolve/:videoId': 'Returns direct audio URL (fast)',
      '/stream/:videoId': 'Streams audio via SABR protocol (always works)',
      '/health': 'Health check',
    },
  }));
});

server.listen(PORT, () => {
  console.log(`[ytresolve] listening on :${PORT}`);
  console.log(`[ytresolve] endpoints:`);
  console.log(`  GET /resolve/:videoId  — direct URL (fast)`);
  console.log(`  GET /stream/:videoId   — SABR audio stream (future-proof)`);
  console.log(`  GET /health            — health check`);
});
