fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 2 {
        return Err("SOURCE OUTPUT".into());
    }
    sortsize_slint_spike::qualify_search(args[0].clone().into(), args[1].clone().into())
}
