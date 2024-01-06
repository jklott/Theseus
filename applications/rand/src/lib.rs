#![no_std]

#[macro_use] 
extern crate app_io;
extern crate rtc;




fn generate_rand() {
    // Xorshift RNG
    
    let now = rtc::read_rtc();

    let mut random = (now.seconds ^ now.minutes ^ now.hours ^ now.days ^ now.months ^ now.years) as u64;
    let mut gen_u64 = || {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        random
    };

    println!("{}", gen_u64());
    
}

pub fn main() {
    generate_rand();
}