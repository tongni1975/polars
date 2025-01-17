use arrow::array::BooleanArray;
use arrow::bitmap::utils::BitmapIter;
use arrow::bitmap::MutableBitmap;
use arrow::datatypes::ArrowDataType;
use polars_error::PolarsResult;

use super::utils;
use super::utils::{extend_from_decoder, Decoder, ExactSize};
use crate::parquet::encoding::hybrid_rle::gatherer::HybridRleGatherer;
use crate::parquet::encoding::hybrid_rle::HybridRleDecoder;
use crate::parquet::encoding::Encoding;
use crate::parquet::error::ParquetResult;
use crate::parquet::page::{split_buffer, DataPage, DictPage};
use crate::read::deserialize::utils::filter::Filter;
use crate::read::deserialize::utils::{BatchableCollector, PageValidity};

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum StateTranslation<'a> {
    Plain(BitmapIter<'a>),
    Rle(HybridRleDecoder<'a>),
}

impl<'a> utils::StateTranslation<'a, BooleanDecoder> for StateTranslation<'a> {
    type PlainDecoder = BitmapIter<'a>;

    fn new(
        _decoder: &BooleanDecoder,
        page: &'a DataPage,
        _dict: Option<&'a <BooleanDecoder as Decoder>::Dict>,
        page_validity: Option<&PageValidity<'a>>,
        _filter: Option<&Filter<'a>>,
    ) -> PolarsResult<Self> {
        let values = split_buffer(page)?.values;

        match page.encoding() {
            Encoding::Plain => {
                let num_values = if page_validity.is_some() {
                    // @NOTE: We overestimate the amount of values here, but in the V1
                    // specification we don't really have a way to know the number of valid items.
                    // Without traversing the list.
                    values.len() * u8::BITS as usize
                } else {
                    page.num_values()
                };
                Ok(Self::Plain(BitmapIter::new(values, 0, num_values)))
            },
            Encoding::Rle => {
                // @NOTE: For a nullable list, we might very well overestimate the amount of
                // values, but we never collect those items. We don't really have a way to now the
                // number of valid items in the V1 specification.

                // For RLE boolean values the length in bytes is pre-pended.
                // https://github.com/apache/parquet-format/blob/e517ac4dbe08d518eb5c2e58576d4c711973db94/Encodings.md#run-length-encoding--bit-packing-hybrid-rle--3
                let (_len_in_bytes, values) = values.split_at(4);
                Ok(Self::Rle(HybridRleDecoder::new(
                    values,
                    1,
                    page.num_values(),
                )))
            },
            _ => Err(utils::not_implemented(page)),
        }
    }

    fn len_when_not_nullable(&self) -> usize {
        match self {
            Self::Plain(v) => v.len(),
            Self::Rle(v) => v.len(),
        }
    }

    fn skip_in_place(&mut self, n: usize) -> ParquetResult<()> {
        if n == 0 {
            return Ok(());
        }

        // @TODO: Add a skip_in_place on BitmapIter
        match self {
            Self::Plain(t) => _ = t.nth(n - 1),
            Self::Rle(t) => t.skip_in_place(n)?,
        }

        Ok(())
    }

    fn extend_from_state(
        &mut self,
        decoder: &mut BooleanDecoder,
        decoded: &mut <BooleanDecoder as Decoder>::DecodedState,
        page_validity: &mut Option<PageValidity<'a>>,
        additional: usize,
    ) -> ParquetResult<()> {
        match self {
            Self::Plain(page_values) => decoder.decode_plain_encoded(
                decoded,
                page_values,
                page_validity.as_mut(),
                additional,
            )?,
            Self::Rle(page_values) => {
                let (values, validity) = decoded;
                match page_validity {
                    None => page_values.gather_n_into(values, additional, &BitmapGatherer)?,
                    Some(page_validity) => utils::extend_from_decoder(
                        validity,
                        page_validity,
                        Some(additional),
                        values,
                        BitmapCollector(page_values),
                    )?,
                }
            },
        }

        Ok(())
    }
}

struct BitmapGatherer;
impl HybridRleGatherer<u32> for BitmapGatherer {
    type Target = MutableBitmap;

    fn target_reserve(&self, target: &mut Self::Target, n: usize) {
        target.reserve(n);
    }

    fn target_num_elements(&self, target: &Self::Target) -> usize {
        target.len()
    }

    fn hybridrle_to_target(&self, value: u32) -> ParquetResult<u32> {
        Ok(value)
    }

    fn gather_one(&self, target: &mut Self::Target, value: u32) -> ParquetResult<()> {
        target.push(value != 0);
        Ok(())
    }

    fn gather_repeated(
        &self,
        target: &mut Self::Target,
        value: u32,
        n: usize,
    ) -> ParquetResult<()> {
        target.extend_constant(n, value != 0);
        Ok(())
    }

    // @TODO: The slice impl here can speed some stuff up
}
struct BitmapCollector<'a, 'b>(&'b mut HybridRleDecoder<'a>);
impl<'a, 'b> BatchableCollector<u32, MutableBitmap> for BitmapCollector<'a, 'b> {
    fn reserve(target: &mut MutableBitmap, n: usize) {
        target.reserve(n);
    }

    fn push_n(&mut self, target: &mut MutableBitmap, n: usize) -> ParquetResult<()> {
        self.0.gather_n_into(target, n, &BitmapGatherer)
    }

    fn push_n_nulls(&mut self, target: &mut MutableBitmap, n: usize) -> ParquetResult<()> {
        target.extend_constant(n, false);
        Ok(())
    }
}

impl ExactSize for (MutableBitmap, MutableBitmap) {
    fn len(&self) -> usize {
        self.0.len()
    }
}

impl ExactSize for () {
    fn len(&self) -> usize {
        0
    }
}

pub(crate) struct BooleanDecoder;

impl Decoder for BooleanDecoder {
    type Translation<'a> = StateTranslation<'a>;
    type Dict = ();
    type DecodedState = (MutableBitmap, MutableBitmap);

    fn with_capacity(&self, capacity: usize) -> Self::DecodedState {
        (
            MutableBitmap::with_capacity(capacity),
            MutableBitmap::with_capacity(capacity),
        )
    }

    fn deserialize_dict(&self, _: DictPage) -> Self::Dict {}

    fn decode_plain_encoded<'a>(
        &mut self,
        (values, validity): &mut Self::DecodedState,
        page_values: &mut <Self::Translation<'a> as utils::StateTranslation<'a, Self>>::PlainDecoder,
        page_validity: Option<&mut PageValidity<'a>>,
        limit: usize,
    ) -> ParquetResult<()> {
        match page_validity {
            None => page_values.collect_n_into(values, limit),
            Some(page_validity) => {
                extend_from_decoder(validity, page_validity, Some(limit), values, page_values)?
            },
        }

        Ok(())
    }

    fn decode_dictionary_encoded<'a>(
        &mut self,
        _decoded: &mut Self::DecodedState,
        _page_values: &mut HybridRleDecoder<'a>,
        _page_validity: Option<&mut PageValidity<'a>>,
        _dict: &Self::Dict,
        _limit: usize,
    ) -> ParquetResult<()> {
        unimplemented!()
    }

    fn finalize(
        &self,
        data_type: ArrowDataType,
        (values, validity): Self::DecodedState,
    ) -> ParquetResult<Box<dyn arrow::array::Array>> {
        Ok(Box::new(BooleanArray::new(
            data_type,
            values.into(),
            validity.into(),
        )))
    }

    fn finalize_dict_array<K: arrow::array::DictionaryKey>(
        &self,
        _data_type: ArrowDataType,
        _dict: Self::Dict,
        _decoded: (Vec<K>, Option<arrow::bitmap::Bitmap>),
    ) -> ParquetResult<arrow::array::DictionaryArray<K>> {
        unimplemented!()
    }
}

impl utils::NestedDecoder for BooleanDecoder {
    fn validity_extend(
        _: &mut utils::State<'_, Self>,
        (_, validity): &mut Self::DecodedState,
        value: bool,
        n: usize,
    ) {
        validity.extend_constant(n, value);
    }

    fn values_extend_nulls(
        _: &mut utils::State<'_, Self>,
        (values, _): &mut Self::DecodedState,
        n: usize,
    ) {
        values.extend_constant(n, false);
    }
}
