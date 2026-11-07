/*
 copyright: (c) 2013-2020 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

use std::io;
use std::io::Read;

/// Wrap a Read so that we store a copy of what was read.
/// Used for re-trying reads when we don't know what to expect from the stream.
/// Bytes are returned in source order, with buffered bytes replayed before reading
/// the source. Reads may be short even when the caller provides a larger buffer.
/// An inner-reader error leaves the buffer and position unchanged for a retry.
/// Empty reads return zero without accessing the source.
pub struct RetryReader<'a, R: Read> {
    fd: &'a mut R,
    buf: Vec<u8>,
    i: usize,
}

impl<'a, R: Read> RetryReader<'a, R> {
    /// Wrap a source with an initially empty replay buffer.
    pub fn new(fd: &'a mut R) -> RetryReader<'a, R> {
        RetryReader {
            fd,
            buf: vec![],
            i: 0,
        }
    }

    /// Set the replay position, clamping offsets to the buffered length.
    pub fn set_position(&mut self, offset: usize) {
        if offset <= self.buf.len() {
            self.i = offset
        } else {
            self.i = self.buf.len()
        }
    }

    /// Return the current position within the buffered bytes.
    pub fn position(&self) -> usize {
        self.i
    }

    /// Append successfully read source bytes and advance the position.
    fn read_and_buffer(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let nr = self.fd.read(buf)?;
        self.buf.extend_from_slice(&buf[0..nr]);
        self.i += nr;
        Ok(nr)
    }
}

impl<R: Read> Read for RetryReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.i < self.buf.len() {
            let bytes_copied = (&self.buf[self.i..]).read(buf)?;
            self.i += bytes_copied;
            return Ok(bytes_copied);
        }
        self.read_and_buffer(buf)
    }
}

/// A Read that returns EOF after at most the configured number of source bytes.
/// Requests never exceed the remaining allowance. Empty reads and reads at the
/// limit return zero without accessing the source. Reads may be short; errors
/// leave the count unchanged so callers can retry without losing their allowance.
pub struct BoundReader<'a, R: Read> {
    fd: &'a mut R,
    max_len: u64,
    read_so_far: u64,
}

impl<'a, R: Read> BoundReader<'a, R> {
    /// Wrap a source with a byte limit and an initially zero read count.
    pub fn from_reader(reader: &'a mut R, max_len: u64) -> BoundReader<'a, R> {
        BoundReader {
            fd: reader,
            max_len,
            read_so_far: 0,
        }
    }

    /// Return the number of bytes successfully read from the source.
    pub fn num_read(&self) -> u64 {
        self.read_so_far
    }
}

impl<R: Read> Read for BoundReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let max_read = (buf.len() as u64).min(self.max_len.saturating_sub(self.read_so_far));
        if max_read == 0 {
            return Ok(0);
        }
        let nr = self.fd.read(&mut buf[..max_read as usize])?;
        self.read_so_far += nr as u64;
        Ok(nr)
    }
}

/// A Read that will log everything it reads
pub struct LogReader<'a, R: Read> {
    fd: &'a mut R,
    reads: Vec<Vec<u8>>,
}

impl<'a, R: Read> LogReader<'a, R> {
    pub fn from_reader(fd: &'a mut R) -> LogReader<'a, R> {
        LogReader { fd, reads: vec![] }
    }

    pub fn log(&self) -> &Vec<Vec<u8>> {
        &self.reads
    }
}

impl<R: Read> Read for LogReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let nr = self.fd.read(buf)?;
        let read = buf[0..nr].to_vec();
        self.reads.push(read);
        Ok(nr)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::collections::VecDeque;

    /// A deterministic source that retains chunk tails and records source requests.
    struct ScriptedReader {
        steps: VecDeque<io::Result<Vec<u8>>>,
        requests: Vec<usize>,
    }

    impl ScriptedReader {
        fn new(steps: Vec<io::Result<Vec<u8>>>) -> Self {
            Self {
                steps: steps.into(),
                requests: vec![],
            }
        }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.requests.push(buf.len());
            let chunk = self.steps.pop_front().expect("unexpected source access")?;
            let n = buf.len().min(chunk.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            if n < chunk.len() {
                self.steps.push_front(Ok(chunk[n..].to_vec()));
            }
            Ok(n)
        }
    }

    #[test]
    /// Replaying bytes must succeed before an unavailable source is consulted.
    fn retry_scripted_errors_and_replay() {
        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted] {
            let mut source = ScriptedReader::new(vec![
                Ok(vec![1, 2]),
                Err(kind.into()),
                Ok(vec![3, 4]),
                Ok(vec![]),
            ]);
            let mut reader = RetryReader::new(&mut source);
            let mut buf = [0; 8];
            assert_eq!(reader.read(&mut []).unwrap(), 0);
            assert_eq!(reader.read(&mut buf).unwrap(), 2);
            reader.set_position(0);
            assert_eq!(reader.read(&mut []).unwrap(), 0);
            assert_eq!(reader.read(&mut buf).unwrap(), 2);
            assert_eq!(&buf[..2], &[1, 2]);
            assert_eq!(reader.fd.requests.len(), 1);
            assert_eq!(reader.read(&mut buf).unwrap_err().kind(), kind);
            assert_eq!(reader.position(), 2);
            assert_eq!(reader.buf, [1, 2]);
            assert_eq!(reader.read(&mut buf).unwrap(), 2);
            assert_eq!(&buf[..2], &[3, 4]);
            reader.set_position(0);
            reader.read_exact(&mut buf[..4]).unwrap();
            assert_eq!(&buf[..4], &[1, 2, 3, 4]);
            reader.set_position(usize::MAX);
            assert_eq!(reader.position(), 4);
            assert_eq!(reader.read(&mut buf).unwrap(), 0);
            assert_eq!(reader.fd.requests, [8, 8, 8, 8]);
        }
    }

    #[test]
    /// read_exact must join a partial replay with uneven new source chunks.
    fn retry_partial_replay_continues_source() {
        let mut source = ScriptedReader::new(vec![Ok(vec![1, 2, 3]), Ok(vec![4]), Ok(vec![5, 6])]);
        let mut reader = RetryReader::new(&mut source);
        let mut prefix = [0; 2];
        reader.read_exact(&mut prefix).unwrap();
        reader.set_position(0);
        let mut all = [0; 6];
        reader.read_exact(&mut all).unwrap();
        assert_eq!(all, [1, 2, 3, 4, 5, 6]);
        assert_eq!(reader.position(), 6);
        assert_eq!(reader.fd.requests, [2, 4, 3, 2]);
    }

    #[test]
    /// Limits clamp source requests, including after errors and near u64::MAX.
    fn bound_scripted_limits_and_errors() {
        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted] {
            let mut source =
                ScriptedReader::new(vec![Ok(vec![1, 2]), Err(kind.into()), Ok(vec![3, 4, 5])]);
            let mut reader = BoundReader::from_reader(&mut source, 4);
            let mut buf = [0; 8];
            assert_eq!(reader.read(&mut []).unwrap(), 0);
            assert_eq!(reader.read(&mut buf).unwrap(), 2);
            assert_eq!(reader.read(&mut buf).unwrap_err().kind(), kind);
            assert_eq!(reader.num_read(), 2);
            assert_eq!(reader.read(&mut buf).unwrap(), 2);
            assert_eq!(&buf[..2], &[3, 4]);
            assert_eq!(reader.num_read(), 4);
            assert_eq!(reader.read(&mut buf).unwrap(), 0);
            assert_eq!(reader.fd.requests, [4, 2, 2]);
        }
        let mut source = ScriptedReader::new(vec![]);
        let mut reader = BoundReader::from_reader(&mut source, 0);
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        assert!(reader.fd.requests.is_empty());

        let mut source = ScriptedReader::new(vec![Ok(vec![9, 10])]);
        let mut reader = BoundReader::from_reader(&mut source, u64::MAX);
        // Seed the count to exercise arithmetic near the limit without huge I/O.
        reader.read_so_far = u64::MAX - 1;
        assert_eq!(reader.read(&mut [0; 8]).unwrap(), 1);
        assert_eq!(reader.num_read(), u64::MAX);
        assert_eq!(reader.fd.requests, [1]);
    }

    #[test]
    /// The mempool wrapper composition preserves a bounded prefix across retries.
    fn retry_over_bound_scripted() {
        let mut source = ScriptedReader::new(vec![
            Ok(vec![1, 2]),
            Err(io::ErrorKind::WouldBlock.into()),
            Ok(vec![3]),
            Err(io::ErrorKind::Interrupted.into()),
            Ok(vec![4, 5, 6]),
        ]);
        let mut bound = BoundReader::from_reader(&mut source, 5);
        let mut reader = RetryReader::new(&mut bound);
        let mut output = vec![];
        let mut buf = [0; 8];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(output, [1, 2, 3, 4, 5]);
        reader.set_position(0);
        let mut replay = [0; 5];
        reader.read_exact(&mut replay).unwrap();
        assert_eq!(replay.as_slice(), output);
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
        assert_eq!(reader.fd.num_read(), 5);
        assert_eq!(reader.fd.fd.requests, [5, 3, 3, 2, 2]);
        assert_eq!(reader.fd.fd.steps.front().unwrap().as_ref().unwrap(), &[6]);
    }

    #[test]
    fn test_retry_reader() {
        let buf = vec![1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4];
        let mut cursor = io::Cursor::new(&buf);
        let mut retry_reader = RetryReader::new(&mut cursor);

        let mut empty_buf = [];
        let nr = retry_reader.read(&mut empty_buf).unwrap();
        assert_eq!(nr, 0);

        for i in 1..5 {
            let mut read_buf = [0u8; 3];
            retry_reader.read_exact(&mut read_buf).unwrap();
            assert_eq!(read_buf, [i as u8, i as u8, i as u8]);
        }

        let mut tmp_buf = [0u8; 3];
        let e = retry_reader.read_exact(&mut tmp_buf);
        assert!(e.is_err(), "{:?}", e);
        assert!(
            format!("{:?}", e.as_ref().unwrap_err())
                .find("failed to fill whole buffer")
                .is_some(),
            "{:?}",
            e
        );

        let res = retry_reader.read(&mut tmp_buf);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 0);

        retry_reader.set_position(0);

        for i in 1..5 {
            let mut read_buf = [0u8; 3];
            retry_reader.read_exact(&mut read_buf).unwrap();
            assert_eq!(read_buf, [i as u8, i as u8, i as u8]);
        }
    }

    #[test]
    fn test_bound_reader() {
        let tests = [
            ("aaaaaaaaaa", 10, "aaaaaaaaaa"),
            ("bbbbbbbbbb", 9, "bbbbbbbbb"),
            ("cccccccccc", 1, "c"),
            ("dddddddddd", 0, ""),
        ];

        // read_to_end
        for (data, len, expected) in tests.iter() {
            let mut cursor = io::Cursor::new(data.as_bytes());
            let mut reader = BoundReader::from_reader(&mut cursor, *len as u64);
            let mut buf = vec![];
            reader.read_to_end(&mut buf).unwrap();
            assert_eq!(buf.len(), *len);
            assert_eq!(buf, expected.as_bytes().to_vec());

            // should EOF once length is exceeded
            let mut buf2 = vec![0u8; *len];
            let nr = reader.read(&mut buf2).unwrap();
            assert_eq!(nr, 0);
            assert_eq!(buf2, vec![0u8; *len]);
        }

        // read piecemeal
        for (data, len, expected) in tests.iter() {
            let mut cursor = io::Cursor::new(data.as_bytes());
            let mut reader = BoundReader::from_reader(&mut cursor, *len as u64);
            let mut buf = vec![];

            for i in 0..*len {
                let mut tmp = [0u8; 1];
                let nr = reader.read(&mut tmp).unwrap();
                assert_eq!(nr, 1);
                buf.extend_from_slice(&tmp);
            }

            assert_eq!(buf.len(), *len);
            assert_eq!(buf, expected.as_bytes().to_vec());

            // should EOF once length is exceeded
            let mut buf2 = vec![0u8; *len];
            let nr = reader.read(&mut buf2).unwrap();
            assert_eq!(nr, 0);
            assert_eq!(buf2, vec![0u8; *len]);
        }
    }
}
