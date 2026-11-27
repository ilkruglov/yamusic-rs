//! Трек играет, пока докачивается: декодер читает то, что уже пришло, и ждёт остальное.
//! Байты идут во временный файл: lossless весит 30–40 МБ, а кэш файла держит ядро, не процесс.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use reqwest::blocking::Response;

/// Сколько ждать следующих байтов, прежде чем считать загрузку оборвавшейся. Дольше, чем
/// загрузчик тратит на все попытки переподключиться.
const STALL: Duration = Duration::from_secs(90);

/// Сколько раз подряд переподключаться, если байты после этого так и не пошли.
const RETRIES: u32 = 4;

/// Длина всего файла и граница тела этого HTTP-ответа (исключая последний байт).
struct Segment {
    total: Option<u64>,
    end: Option<u64>,
}

impl Segment {
    fn from_response(resp: &Response, from: u64) -> Result<Self, String> {
        let length = resp.content_length();
        if from == 0 && resp.status() == reqwest::StatusCode::OK {
            return Ok(Self { total: length, end: length });
        }
        let invalid = "сервер вернул неверный диапазон байтов";
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT { return Err(invalid.into()); }
        let raw = resp.headers().get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("bytes ")).ok_or(invalid)?;
        let (range, total) = raw.split_once('/').ok_or(invalid)?;
        let (start, last) = range.split_once('-').ok_or(invalid)?;
        let number = |s: &str| s.parse::<u64>().map_err(|_| invalid);
        let end = number(last)?.checked_add(1).ok_or(invalid)?;
        let total = number(total)?;
        if number(start)? != from || end <= from || end > total || length.is_some_and(|n| n != end - from) {
            return Err(invalid.into());
        }
        Ok(Self { total: Some(total), end: Some(end) })
    }
}

#[derive(Default)]
struct State {
    /// Сколько байтов уже в файле.
    have: u64,
    len: Option<u64>,
    done: bool,
    err: Option<String>,
}

pub struct Buf {
    file: File,
    state: Mutex<State>,
    cv: Condvar,
}

impl Buf {
    fn new() -> io::Result<Buf> {
        Ok(Buf { file: temp_file()?, state: Mutex::default(), cv: Condvar::new() })
    }

    /// Качает ответ в фоне. Если соединение оборвалось, `reopen` открывает файл заново с нужного
    /// байта. Если буфер больше никому не нужен (трек пропустили), загрузка останавливается.
    pub fn download<F>(mut resp: Response, mut reopen: F) -> Result<Arc<Buf>, String>
    where
        F: FnMut(u64) -> Result<Response, String> + Send + 'static,
    {
        let buf = Arc::new(Buf::new().map_err(|e| format!("временный файл: {e}"))?);
        let mut segment = Segment::from_response(&resp, 0)?;
        let etag = resp.headers().get(reqwest::header::ETAG)
            .filter(|v| v.as_bytes().starts_with(b"\"")).cloned();
        buf.state.lock().unwrap().len = segment.total;
        let b = buf.clone();
        std::thread::spawn(move || {
            let mut chunk = vec![0u8; 64 * 1024];
            let mut failures = 0;
            loop {
                if Arc::strong_count(&b) == 1 {
                    return;
                }
                let have = b.state.lock().unwrap().have;
                let read = resp.read(&mut chunk).and_then(|n| {
                    if n == 0 && segment.total.is_some_and(|len| have < len) {
                        Err(io::Error::new(io::ErrorKind::UnexpectedEof, "трек получен не полностью"))
                    } else if segment.end.is_some_and(|end| n as u64 > end.saturating_sub(have)) {
                        Err(io::Error::new(io::ErrorKind::InvalidData, "ответ выходит за пределы диапазона"))
                    } else {
                        Ok(n)
                    }
                });
                let res = match read {
                    Ok(n) => write_at(&b.file, &chunk[..n], have).map(|()| n).map_err(|e| format!("временный файл: {e}")),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        crate::config::log(format!("загрузка оборвалась на {have} байте: {e}"));
                        let mut resume = |from| {
                            let next = reopen(from)?;
                            let mut part = Segment::from_response(&next, from)?;
                            if segment.total.zip(part.total).is_some_and(|(a, b)| a != b)
                                || etag.as_ref().is_some_and(|tag| next.headers().get(reqwest::header::ETAG) != Some(tag)) {
                                return Err("версия файла изменилась".into());
                            }
                            part.total = part.total.or(segment.total);
                            Ok((next, part))
                        };
                        match reconnect(&b, &mut resume, have, &mut failures) {
                            Ok((next, part)) => {
                                b.state.lock().unwrap().len = part.total;
                                segment = part;
                                resp = next;
                                continue;
                            }
                            Err(()) => Err(format!("сеть: {e}")),
                        }
                    }
                };
                let mut st = b.state.lock().unwrap();
                match res {
                    Ok(0) => st.done = true,
                    Ok(n) => {
                        st.have += n as u64;
                        failures = 0;
                    }
                    Err(e) => {
                        st.err = Some(e);
                        st.done = true;
                    }
                }
                let done = st.done;
                drop(st);
                b.cv.notify_all();
                if done {
                    return;
                }
            }
        });
        Ok(buf)
    }

    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: &[u8]) -> Arc<Self> {
        let buf = Arc::new(Buf::new().unwrap());
        write_at(&buf.file, bytes, 0).unwrap();
        {
            let mut state = buf.state.lock().unwrap();
            state.have = bytes.len() as u64;
            state.len = Some(state.have);
            state.done = true;
        }
        buf
    }

    pub fn reader(self: &Arc<Self>) -> Reader {
        Reader { buf: self.clone(), pos: 0 }
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.state.lock().unwrap().err.clone()
    }

    /// Загрузка закончилась, успешно или нет.
    pub fn done(&self) -> bool {
        self.state.lock().unwrap().done
    }

    /// Ждёт, пока придёт больше `pos` байтов или загрузка закончится.
    fn wait_past(&self, pos: u64) -> io::Result<MutexGuard<'_, State>> {
        let mut st = self.state.lock().unwrap();
        while st.have <= pos && !st.done {
            let (next, t) = self.cv.wait_timeout(st, STALL).unwrap();
            st = next;
            if t.timed_out() && st.have <= pos && !st.done {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "трек перестал загружаться"));
            }
        }
        Ok(st)
    }
}

/// Ждёт и переоткрывает загрузку с байта `from`. `Err`, если попытки кончились или трек уже не нужен.
fn reconnect<F>(buf: &Arc<Buf>, reopen: &mut F, from: u64, failures: &mut u32) -> Result<(Response, Segment), ()>
where
    F: FnMut(u64) -> Result<(Response, Segment), String>,
{
    while *failures < RETRIES {
        *failures += 1;
        std::thread::sleep(Duration::from_secs(1 << (*failures - 1)));
        if Arc::strong_count(buf) == 1 {
            return Err(());
        }
        match reopen(from) {
            Ok(resp) => return Ok(resp),
            Err(e) => crate::config::log(format!("докачка с {from} байта: {e}")),
        }
    }
    Err(())
}

/// Файл исчезает сам, когда его закроют, даже если приложение упадёт.
fn temp_file() -> io::Result<File> {
    let dir = crate::config::cache_dir().join("stream");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{:x}-{:x}", std::process::id(), rand_suffix()));
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
        const FILE_SHARE_ALL: u32 = 0x7;
        opts.custom_flags(FILE_FLAG_DELETE_ON_CLOSE).share_mode(FILE_SHARE_ALL);
    }
    let file = opts.open(&path)?;
    #[cfg(unix)]
    std::fs::remove_file(&path)?;
    Ok(file)
}

fn rand_suffix() -> u128 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    h.finish() as u128
}

#[cfg(unix)]
fn write_at(f: &File, data: &[u8], at: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, data, at)
}

#[cfg(windows)]
fn write_at(f: &File, mut data: &[u8], mut at: u64) -> io::Result<()> {
    while !data.is_empty() {
        let n = std::os::windows::fs::FileExt::seek_write(f, data, at)?;
        data = &data[n..];
        at += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(f: &File, out: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, out, at)
}

#[cfg(windows)]
fn read_at(f: &File, out: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, out, at)
}

#[derive(Clone)]
pub struct Reader {
    buf: Arc<Buf>,
    pos: u64,
}

impl symphonia::core::io::MediaSource for Reader {
    fn is_seekable(&self) -> bool { true }

    fn byte_len(&self) -> Option<u64> {
        let st = self.buf.state.lock().unwrap();
        st.len.or_else(|| (st.done && st.err.is_none()).then_some(st.have))
    }
}

impl Read for Reader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let st = self.buf.wait_past(self.pos)?;
        let have = st.have;
        if self.pos >= have {
            return match &st.err {
                Some(e) => Err(io::Error::other(e.clone())),
                None => Ok(0),
            };
        }
        drop(st);
        let n = out.len().min((have - self.pos) as usize);
        let n = read_at(&self.buf.file, &mut out[..n], self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Reader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => {
                let len = {
                    let st = self.buf.state.lock().unwrap();
                    st.len
                };
                let len = match len {
                    Some(n) => n,
                    None => self.buf.wait_past(u64::MAX)?.have,
                };
                len.checked_add_signed(d)
            }
        };
        self.pos = pos.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "позиция до начала файла"))?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(b: &Buf, data: &[u8], done: bool) {
        let mut st = b.state.lock().unwrap();
        write_at(&b.file, data, st.have).unwrap();
        st.have += data.len() as u64;
        st.done = done;
        drop(st);
        b.cv.notify_all();
    }

    #[test]
    fn resume_appends_only_the_matching_range_and_representation() {
        use std::io::Write;
        use std::net::TcpListener;
        fn segment(range: &str, etag: &str, bytes: &'static str) -> Response {
            let server = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let addr = server.local_addr().unwrap();
            let reply = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {range}\r\nETag: \"{etag}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{bytes}", bytes.len());
            std::thread::spawn(move || {
                let (mut client, _) = server.accept().unwrap();
                let mut headers = std::io::BufReader::new(&client);
                let mut line = String::new();
                while std::io::BufRead::read_line(&mut headers, &mut line).unwrap() > 0 {
                    if line == "\r\n" { break; }
                    line.clear();
                }
                client.write_all(reply.as_bytes()).unwrap();
            });
            reqwest::blocking::Client::builder().no_proxy().build().unwrap().get(format!("http://{addr}")).send().unwrap()
        }
        let mut replies = [("2-4/6", "v1", "BAD"), ("3-5/7", "v1", "BAD"), ("3-5/6", "v2", "BAD"), ("3-5/6", "v1", "def")].into_iter();
        let buf = Buf::download(segment("0-2/6", "v1", "abc"), move |from| {
            assert_eq!(from, 3);
            let (range, etag, bytes) = replies.next().unwrap();
            Ok(segment(range, etag, bytes))
        }).unwrap();
        let mut all = Vec::new();
        buf.reader().read_to_end(&mut all).unwrap();
        assert_eq!(all, b"abcdef");
    }

    #[test]
    fn reader_waits_for_data() {
        let buf = Arc::new(Buf::new().unwrap());
        let mut r = buf.reader();
        let b = buf.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            push(&b, b"abc", false);
            std::thread::sleep(Duration::from_millis(50));
            push(&b, b"de", true);
        });
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        t.join().unwrap();
        assert_eq!(all, b"abcde");
        assert_eq!(r.seek(SeekFrom::End(-2)).unwrap(), 3);
        let mut tail = String::new();
        r.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "de");
    }
}
