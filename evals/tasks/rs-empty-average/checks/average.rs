use stats::{above_average, average};

#[test]
fn empty_is_none() {
    assert_eq!(average(&[]), None);
}

#[test]
fn mean_of_values() {
    assert_eq!(average(&[1.0, 2.0, 6.0]), Some(3.0));
}

#[test]
fn above_average_still_works() {
    assert_eq!(above_average(&[1.0, 2.0, 6.0]), vec![6.0]);
    assert_eq!(above_average(&[1.0, 2.0, 3.0]), vec![3.0]);
    assert_eq!(above_average(&[4.0, 4.0]), Vec::<f64>::new());
    assert!(above_average(&[]).is_empty());
}
