use std::{fmt, iter};

/// A collection that always has a first element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T> {
    first: T,
    rest: Vec<T>,
}

impl<T> NonEmpty<T> {
    /// Destructure a collection that has a guaranteed first element.
    #[must_use]
    pub fn into_parts(self) -> (T, Vec<T>) {
        (self.first, self.rest)
    }

    /// Iterate over the elements in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        iter::once(&self.first).chain(&self.rest)
    }

    /// Collect the elements into a vector.
    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        iter::once(self.first).chain(self.rest).collect()
    }

    /// Map each element while preserving the guarantee that there is a first element.
    #[must_use]
    pub fn map<U>(self, mut convert: impl FnMut(T) -> U) -> NonEmpty<U> {
        NonEmpty {
            first: convert(self.first),
            rest: self.rest.into_iter().map(convert).collect(),
        }
    }

    /// Convert each element while preserving the guaranteed first element.
    ///
    /// # Errors
    ///
    /// Returns the first error from the conversion.
    pub fn try_map<U, E>(
        self,
        mut convert: impl FnMut(T) -> Result<U, E>,
    ) -> Result<NonEmpty<U>, E> {
        Ok(NonEmpty {
            first: convert(self.first)?,
            rest: self
                .rest
                .into_iter()
                .map(convert)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl<T> From<(T, Vec<T>)> for NonEmpty<T> {
    fn from((first, rest): (T, Vec<T>)) -> Self {
        Self { first, rest }
    }
}

/// A collection lacked the required first element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyCollection;

impl fmt::Display for EmptyCollection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("expected at least one element")
    }
}

impl std::error::Error for EmptyCollection {}

impl<T> TryFrom<Vec<T>> for NonEmpty<T> {
    type Error = EmptyCollection;

    fn try_from(values: Vec<T>) -> Result<Self, Self::Error> {
        let mut items = values.into_iter();
        Ok(Self {
            first: items.next().ok_or(EmptyCollection)?,
            rest: items.collect(),
        })
    }
}
