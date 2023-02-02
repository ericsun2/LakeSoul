use std::fmt::Debug;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use crate::sorted_merge::sort_key_range::{SortKeyBatchRange, SortKeyArrayRange, SortKeyBatchRanges};
use crate::sorted_merge::merge_operator::{MergeOperator, MergeResult};

use arrow::{error::Result as ArrowResult, 
    error::ArrowError,  
    record_batch::RecordBatch, 
    datatypes::{Field, SchemaRef, DataType},
    array::{
        make_array as make_arrow_array, Array, ArrayRef, 
        ArrayBuilder, PrimitiveBuilder, GenericStringBuilder, 
        MutableArrayData
    },
};
use arrow_array::types::*;


#[derive(Debug)]
pub enum RangeCombiner {
    MinHeapSortKeyBatchRangeCombiner(MinHeapSortKeyBatchRangeCombiner),
}

impl RangeCombiner {
    pub fn new(
        schema: SchemaRef,
        streams_num:usize,
        target_batch_size: usize) -> Self {
        RangeCombiner::MinHeapSortKeyBatchRangeCombiner(MinHeapSortKeyBatchRangeCombiner::new(schema, streams_num, target_batch_size))
    }

    pub fn push_range(&mut self, range: Reverse<SortKeyBatchRange>) {
        match self {
            RangeCombiner::MinHeapSortKeyBatchRangeCombiner(combiner) => combiner.push(range)
        };
    }

    pub fn poll_result(&mut self) -> RangeCombinerResult {
        match self {
            RangeCombiner::MinHeapSortKeyBatchRangeCombiner(combiner) => combiner.poll_result()
        }
    }
}

#[derive(Debug)]
pub enum RangeCombinerResult {
    None,
    Err(ArrowError),
    Range(Reverse<SortKeyBatchRange>),
    RecordBatch(ArrowResult<RecordBatch>),
}

#[derive(Debug)]
pub struct MinHeapSortKeyBatchRangeCombiner{
    schema: SchemaRef,
    heap: BinaryHeap<Reverse<SortKeyBatchRange>>,
    in_progress: Vec<SortKeyBatchRanges>,
    target_batch_size: usize,
    current_sort_key_range: SortKeyBatchRanges,
    merge_operator: Vec<MergeOperator>,
}

impl MinHeapSortKeyBatchRangeCombiner{
    pub fn new(
        schema: SchemaRef,
        streams_num: usize, 
        target_batch_size: usize) -> Self {
        let new_range = SortKeyBatchRanges::new(schema.clone());
        MinHeapSortKeyBatchRangeCombiner{
            schema: schema.clone(),
            heap: BinaryHeap::with_capacity(streams_num),
            in_progress: vec![],
            target_batch_size: target_batch_size,
            current_sort_key_range: new_range,
            merge_operator: vec![MergeOperator::UseLast; schema.clone().fields().len()],
        }
    }

    pub fn push(&mut self, range: Reverse<SortKeyBatchRange>) {
        self.heap.push(range)
    }

    pub fn poll_result(&mut self) -> RangeCombinerResult  {
        if self.in_progress.len() == self.target_batch_size {
            RangeCombinerResult::RecordBatch(self.build_record_batch())
        } else {
            match self.heap.pop() {
                Some(Reverse(range)) => {
                    if self.current_sort_key_range.match_row(&range) {
                        self.current_sort_key_range.add_range_in_batch(range.clone());
                    } else {
                        self.in_progress.push(self.current_sort_key_range.clone());
                        self.current_sort_key_range = SortKeyBatchRanges::new(self.schema.clone());
                        self.current_sort_key_range.add_range_in_batch(range.clone());
                    }
                    RangeCombinerResult::Range(Reverse(range)) 
                }
                None => {
                    if self.current_sort_key_range.is_empty() && self.in_progress.is_empty() {
                        RangeCombinerResult::None
                    } else {
                        if !self.current_sort_key_range.is_empty() {
                            self.in_progress.push(self.current_sort_key_range.clone());
                            self.current_sort_key_range.set_batch_range(None);
                        }
                        RangeCombinerResult::RecordBatch(self.build_record_batch())
                    }
                }
            }
        }
    }
 
    fn build_record_batch(&mut self) -> ArrowResult<RecordBatch> {
        let columns = self
            .schema
            .fields()
            .iter()
            .enumerate()
            .map(|(column_idx, field)| {
                let capacity = self.in_progress.len();
                let ranges_per_col:Vec<Vec<SortKeyArrayRange>> = self.in_progress
                    .iter()
                    .map(|ranges_per_row| ranges_per_row.column(column_idx))
                    .collect::<Vec<_>>();
                
                let mut flatten_array_ranges = ranges_per_col
                    .clone()
                    .into_iter()
                    .flat_map(|ranges| ranges)
                    .collect::<Vec<SortKeyArrayRange>>();
                    
                flatten_array_ranges.dedup_by_key(|range| range.batch_idx);

                let mut flatten_dedup_arrays = Vec::<ArrayRef>::new();
                let mut batch_idx_to_flatten_idx = HashMap::<usize, usize>::new();

                for i in 0..flatten_array_ranges.len() {
                    flatten_dedup_arrays.push(flatten_array_ranges[i].array());
                    batch_idx_to_flatten_idx.insert(flatten_array_ranges[i].batch_idx, i);
                }
                merge_sort_key_array_ranges(capacity, field, &ranges_per_col, &flatten_dedup_arrays, &batch_idx_to_flatten_idx, self.merge_operator.get(column_idx).unwrap())
            })
            .collect();

        self.in_progress.clear();

        RecordBatch::try_new(self.schema.clone(), columns)
    }
}

fn merge_sort_key_array_ranges(capacity:usize, field: &Field, ranges:&Vec<Vec<SortKeyArrayRange>>, flatten_dedup_arrays:&Vec::<ArrayRef>, batch_idx_to_flatten_idx:&HashMap<usize, usize>, merge_operator:&MergeOperator) ->ArrayRef {
    let data_type = (*field.data_type()).clone();
    let mut append_array_data_builder:Box<dyn ArrayBuilder> = 
        match data_type {
            DataType::Int32 => Box::new(PrimitiveBuilder::<Int32Type>::with_capacity(capacity)),
            DataType::Int64 => Box::new(PrimitiveBuilder::<Int64Type>::with_capacity(capacity)),
            DataType::Utf8 => Box::new(GenericStringBuilder::<i32>::with_capacity(capacity, capacity)),
            _ => {
                if *merge_operator == MergeOperator::UseLast {
                    Box::new(PrimitiveBuilder::<Int32Type>::with_capacity(capacity))
                } else {
                    unimplemented!()
                }
            }
        };
    let append_idx = flatten_dedup_arrays.len();
    let null_idx = append_idx + 1;
    let mut null_counter = 0;
    let mut extend_list:Vec<(usize, usize, usize)> = vec![(null_idx + 1, 0, 0)];

    for i in 0..ranges.len() {
        let ranges_of_row = ranges[i].clone();
        let res = merge_operator.merge(data_type.clone(), &ranges_of_row, &mut append_array_data_builder);
        let (flatten_idx, row_idx) = match res {
            MergeResult::AppendValue => {
                (append_idx, append_array_data_builder.len() - 1)
            }
            MergeResult::AppendNull => {
                if !field.is_nullable() {
                    panic!("{} is not nullable", field);
                }
                null_counter += 1;
                (null_idx, null_counter)
            }
            MergeResult::Extend(batch_idx, row_idx) => {
                (batch_idx_to_flatten_idx[&batch_idx], row_idx)
            }
        };
        if let Some(last_extend) = extend_list.last_mut() {
            if flatten_idx == last_extend.0 && last_extend.2 == row_idx {
                last_extend.2 = last_extend.2 + 1;
            } else {
                extend_list.push((flatten_idx, row_idx, row_idx + 1));
            } 
        }
    }

    let append_array_data = append_array_data_builder.finish().into_data();

    let mut source_array_data = flatten_dedup_arrays.iter().map(|array| array.data()).collect::<Vec<_>>();
    source_array_data.push(&append_array_data);

    let mut array_data = MutableArrayData::new(
        source_array_data,
        field.is_nullable(),
        capacity,
    );

    for i in 1..extend_list.len() {
        let &(flatten_idx, start, end) = extend_list.get(i).unwrap();
        if flatten_idx < null_idx {
            array_data.extend(flatten_idx, start, end);
        } else {
            array_data.extend_nulls(end - start);
        }
    }

    make_arrow_array(array_data.freeze())
}
