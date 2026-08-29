// SPDX-License-Identifier: AGPL-3.0-only
//! Shared SPI-tuple column extraction.
//!
//! Every reader-level projection in this crate reshapes an SPI result row into
//! a typed struct, reading one nullable or `NOT NULL` column at a time. Each
//! such read is the same two-step shape - `row.get::<T>(ordinal)` maps a read
//! failure to an internal error, and a `NOT NULL` column additionally rejects a
//! surprise SQL `NULL` - and repeating it inline per column is what pushes the
//! row readers ([`crate::catalog::stats`], [`crate::catalog::audit`],
//! [`crate::catalog::admin`], [`crate::catalog::neighbors`],
//! [`crate::catalog::source`], [`crate::catalog::export`], and the config sync
//! defaults) into high branch counts.
//!
//! This module is the single, tested home for that pattern. [`column`] and
//! [`required_column`] are the primitives - they take the exact diagnostic
//! strings a caller wants, so every existing error message is preserved
//! verbatim - and [`RowReader`] is a thin convenience over them for the common
//! case where a whole struct shares one read context and one composite-type
//! name.

use std::path::Path;

use pgrx::datum::{FromDatum, IntoDatum};
use pgrx::spi::SpiHeapTupleData;

use crate::errors::CatalogError;

/// Read a nullable column at `ordinal` (1-based) from an SPI row.
///
/// A read failure becomes an internal [`CatalogError`] whose message is
/// `"{read_context}: {error}"`, matching the per-reader `spi_error` helpers this
/// replaces. A SQL `NULL` is returned as `None` - use [`required_column`] for a
/// `NOT NULL` column that must reject one.
///
/// # Errors
///
/// Returns a [`CatalogError`] when the column cannot be read as `T`.
pub(crate) fn column<T: FromDatum + IntoDatum>(
    row: &SpiHeapTupleData<'_>,
    ordinal: usize,
    read_context: &str,
) -> Result<Option<T>, CatalogError> {
    row.get::<T>(ordinal)
        .map_err(|error| CatalogError::internal(format!("{read_context}: {error}"), Path::new("")))
}

/// Read a `NOT NULL` column at `ordinal` (1-based), rejecting a SQL `NULL`.
///
/// The read failure is reported exactly as [`column`] does; a `NULL` value in
/// the column raises an internal [`CatalogError`] carrying `null_message`
/// verbatim, so a violated schema invariant surfaces as `XX000` with the
/// caller's own wording.
///
/// # Errors
///
/// Returns a [`CatalogError`] when the column cannot be read as `T` or is
/// unexpectedly `NULL`.
pub(crate) fn required_column<T: FromDatum + IntoDatum>(
    row: &SpiHeapTupleData<'_>,
    ordinal: usize,
    read_context: &str,
    null_message: &str,
) -> Result<T, CatalogError> {
    column::<T>(row, ordinal, read_context)?
        .ok_or_else(|| CatalogError::internal(null_message.to_owned(), Path::new("")))
}

/// A cursor over one SPI row that applies uniform diagnostics for the common
/// case: every read failure is prefixed with one `read_context`, and a `NULL`
/// in a required column is reported as
/// `"{null_type} column {name} is unexpectedly NULL"`.
///
/// The reader borrows the row, so it is created per row and dropped when the
/// struct it fills is built.
pub(crate) struct RowReader<'a, 'conn> {
    row: &'a SpiHeapTupleData<'conn>,
    read_context: &'static str,
    null_type: &'static str,
}

impl<'a, 'conn> RowReader<'a, 'conn> {
    /// Wrap a row with the read-error context and the composite-type name used
    /// to phrase a `NOT NULL` violation.
    pub(crate) fn new(
        row: &'a SpiHeapTupleData<'conn>,
        read_context: &'static str,
        null_type: &'static str,
    ) -> Self {
        Self {
            row,
            read_context,
            null_type,
        }
    }

    /// Read a nullable column at `ordinal` (1-based).
    ///
    /// # Errors
    ///
    /// Returns a [`CatalogError`] when the column cannot be read as `T`.
    pub(crate) fn optional<T: FromDatum + IntoDatum>(
        &self,
        ordinal: usize,
    ) -> Result<Option<T>, CatalogError> {
        column::<T>(self.row, ordinal, self.read_context)
    }

    /// Read a `NOT NULL` column at `ordinal` (1-based), naming `column` in the
    /// `NULL`-violation message.
    ///
    /// # Errors
    ///
    /// Returns a [`CatalogError`] when the column cannot be read as `T` or is
    /// unexpectedly `NULL`.
    pub(crate) fn required<T: FromDatum + IntoDatum>(
        &self,
        ordinal: usize,
        column: &str,
    ) -> Result<T, CatalogError> {
        required_column::<T>(
            self.row,
            ordinal,
            self.read_context,
            &format!("{} column {column} is unexpectedly NULL", self.null_type),
        )
    }
}
