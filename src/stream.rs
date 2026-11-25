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
        buf.state.lock().unwrap().len = resp.content_length();
        let b = buf.clone();
        std::thread::spawn(move || {
            let mut chunk = vec![0u8; 64 * 1024];
            let mut failures = 0;
            loop {
                if Arc::strong_count(&b) == 1 {
                    return;
                }
                let have = b.state.lock().unwrap().have;
                let res = match resp.read(&mut chunk) {
                    Ok(n) => write_at(&b.file, &chunk[..n], have).map(|()| n).map_err(|e| format!("временный файл: {e}")),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        crate::config::log(format!("загрузка оборвалась на {have} байте: {e}"));
                        match reconnect(&b, &mut reopen, have, &mut failures) {
                            Ok(next) => {
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
fn reconnect<F>(buf: &Arc<Buf>, reopen: &mut F, from: u64, failures: &mut u32) -> Result<Response, ()>
where
    F: FnMut(u64) -> Result<Response, String>,
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

pub struct Reader {
    buf: Arc<Buf>,
    pos: u64,
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
