#![allow(clippy::new_ret_no_self)]
use evtx::{
    EvtxParser, IntoIterChunks, JsonOutput, ParserSettings, SerializedEvtxRecord, XmlOutput,
};

use pyo3::exceptions::{NotImplementedError, RuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3::types::PyString;

use pyo3::PyIterProtocol;
use pyo3_file::PyFileLikeObject;

use std::fs::File;
use std::io;
use std::io::{Read, Seek, SeekFrom};

pub trait ReadSeek: Read + Seek {
    fn tell(&mut self) -> io::Result<u64> {
        self.seek(SeekFrom::Current(0))
    }
}

impl<T: Read + Seek> ReadSeek for T {}

struct PyEvtxError(evtx::err::Error);

impl From<PyEvtxError> for PyErr {
    fn from(err: PyEvtxError) -> Self {
        match err.0 {
            evtx::err::Error::IO {
                source,
                backtrace: _,
            } => source.into(),
            _ => PyErr::new::<RuntimeError, _>(format!("{}", err.0)),
        }
    }
}

#[derive(Copy, Clone, PartialOrd, PartialEq)]
pub enum OutputFormat {
    JSON,
    XML,
}

#[derive(Debug)]
enum FileOrFileLike {
    File(String),
    FileLike(PyFileLikeObject),
}

impl FileOrFileLike {
    pub fn from_pyobject(path_or_file_like: PyObject) -> PyResult<FileOrFileLike> {
        let gil = Python::acquire_gil();
        let py = gil.python();

        // is a path
        if let Ok(string_ref) = path_or_file_like.cast_as::<PyString>(py) {
            return Ok(FileOrFileLike::File(
                string_ref.to_string_lossy().to_string(),
            ));
        }

        // We only need read + seek
        match PyFileLikeObject::with_requirements(path_or_file_like, true, false, true) {
            Ok(f) => Ok(FileOrFileLike::FileLike(f)),
            Err(e) => Err(e),
        }
    }
}

#[pyclass]
/// PyEvtxParser(self, path_or_file_like, /)
/// --
///
/// Returns an instance of the parser.
/// Works on both a path (string), or a file-like object.
pub struct PyEvtxParser {
    inner: Option<EvtxParser<Box<dyn ReadSeek>>>,
}

#[pymethods]
impl PyEvtxParser {
    #[new]
    fn new(obj: &PyRawObject, path_or_file_like: PyObject) -> PyResult<()> {
        let file_or_file_like = FileOrFileLike::from_pyobject(path_or_file_like)?;

        let boxed_read_seek = match file_or_file_like {
            FileOrFileLike::File(s) => {
                let file = File::open(s)?;
                Box::new(file) as Box<dyn ReadSeek>
            }
            FileOrFileLike::FileLike(f) => Box::new(f) as Box<dyn ReadSeek>,
        };

        let parser = EvtxParser::from_read_seek(boxed_read_seek).map_err(PyEvtxError)?;

        obj.init({
            PyEvtxParser {
                inner: Some(parser),
            }
        });

        Ok(())
    }

    /// records(self, /)
    /// --
    ///
    /// Returns an iterator that yields XML records.
    fn records(&mut self) -> PyResult<PyRecordsIterator> {
        self.records_iterator(OutputFormat::XML)
    }

    /// records_json(self, /)
    /// --
    ///
    /// Returns an iterator that yields JSON records.
    fn records_json(&mut self) -> PyResult<PyRecordsIterator> {
        self.records_iterator(OutputFormat::JSON)
    }
}

impl PyEvtxParser {
    fn records_iterator(&mut self, output_format: OutputFormat) -> PyResult<PyRecordsIterator> {
        let inner = match self.inner.take() {
            Some(inner) => inner,
            None => {
                return Err(PyErr::new::<RuntimeError, _>(
                    "PyEvtxParser can only be used once",
                ));
            }
        };

        Ok(PyRecordsIterator {
            inner: inner.into_chunks(),
            records: None,
            settings: ParserSettings::new(),
            output_format,
        })
    }
}

fn record_to_pydict(record: SerializedEvtxRecord, py: Python) -> PyResult<&PyDict> {
    let pyrecord = PyDict::new(py);

    pyrecord.set_item("event_record_id", record.event_record_id)?;
    pyrecord.set_item("timestamp", format!("{}", record.timestamp))?;
    pyrecord.set_item("data", record.data)?;
    Ok(pyrecord)
}

fn record_to_pyobject(
    r: Result<SerializedEvtxRecord, evtx::err::Error>,
    py: Python,
) -> PyResult<PyObject> {
    match r {
        Ok(r) => match record_to_pydict(r, py) {
            Ok(dict) => Ok(dict.to_object(py)),
            Err(e) => Ok(e.to_object(py)),
        },
        Err(e) => Err(PyEvtxError(e).into()),
    }
}

#[pyclass]
pub struct PyRecordsIterator {
    inner: IntoIterChunks<Box<dyn ReadSeek>>,
    records: Option<Vec<Result<SerializedEvtxRecord, evtx::err::Error>>>,
    settings: ParserSettings,
    output_format: OutputFormat,
}

impl PyRecordsIterator {
    fn next(&mut self) -> PyResult<Option<PyObject>> {
        let gil = Python::acquire_gil();
        let py = gil.python();

        loop {
            if let Some(record) = self.records.as_mut().and_then(Vec::pop) {
                return record_to_pyobject(record, py).map(Some);
            }

            let chunk = self.inner.next();

            match chunk {
                None => return Ok(None),
                Some(chunk_result) => match chunk_result {
                    Err(e) => {
                        return Err(PyEvtxError(e).into());
                    }
                    Ok(mut chunk) => {
                        let parsed_chunk = chunk.parse(&self.settings);

                        match parsed_chunk {
                            Err(e) => {
                                return Err(PyEvtxError(e).into());
                            }
                            Ok(mut chunk) => {
                                self.records = match self.output_format {
                                    OutputFormat::XML => Some(
                                        chunk
                                            .iter_serialized_records::<XmlOutput<Vec<u8>>>()
                                            .collect(),
                                    ),
                                    OutputFormat::JSON => Some(
                                        chunk
                                            .iter_serialized_records::<JsonOutput<Vec<u8>>>()
                                            .collect(),
                                    ),
                                };
                            }
                        }
                    }
                },
            }
        }
    }
}

#[pyproto]
impl PyIterProtocol for PyEvtxParser {
    fn __iter__(mut slf: PyRefMut<Self>) -> PyResult<PyRecordsIterator> {
        slf.records()
    }
    fn __next__(_slf: PyRefMut<Self>) -> PyResult<Option<PyObject>> {
        Err(PyErr::new::<NotImplementedError, _>("Using `next()` over `PyEvtxParser` is not supported. Try iterating over `PyEvtxParser(...).records()`"))
    }
}

#[pyproto]
impl PyIterProtocol for PyRecordsIterator {
    fn __iter__(slf: PyRefMut<Self>) -> PyResult<Py<PyRecordsIterator>> {
        Ok(slf.into())
    }
    fn __next__(mut slf: PyRefMut<Self>) -> PyResult<Option<PyObject>> {
        slf.next()
    }
}

// Don't use double quotes ("") inside this docstring, this will crash pyo3.
/// Parses an evtx file.
///
/// This will print each record as an XML string.
///
///```python
/// from evtx import PyEvtxParser
///
/// def main():
///    parser = PyEvtxParser('./samples/Security_short_selected.evtx')
///    for record in parser.records():
///        print(f'Event Record ID: {record['event_record_id']}')
///        print(f'Event Timestamp: {record['timestamp']}')
///        print(record['data'])
///        print('------------------------------------------')
///```
///
/// And this will print each record as a JSON string.
///
/// ```python
/// from evtx import PyEvtxParser
///
/// def main():
///    parser = PyEvtxParser('./samples/Security_short_selected.evtx')
///    for record in parser.records_json():
///        print(f'Event Record ID: {record['event_record_id']}')
///        print(f'Event Timestamp: {record['timestamp']}')
///        print(record['data'])
///        print(f'------------------------------------------')
///```
#[pymodule]
fn evtx(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<PyEvtxParser>()?;
    m.add_class::<PyRecordsIterator>()?;

    Ok(())
}
