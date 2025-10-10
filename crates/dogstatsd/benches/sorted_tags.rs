use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use dogstatsd::metric::SortedTags;

fn benchmark_sorted_tags_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("SortedTags::parse");
    
    let test_cases = vec![
        ("empty", ""),
        ("single", "key1:value1"),
        ("few_tags", "key1:value1,key2:value2,key3:value3"),
        ("typical", "env:production,service:api,version:1.0.0,host:server01,region:us-east-1"),
        ("many_tags", "a:1,b:2,c:3,d:4,e:5,f:6,g:7,h:8,i:9,j:10,k:11,l:12,m:13,n:14,o:15"),
        ("unsorted", "z:26,a:1,y:25,b:2,x:24,c:3,w:23,d:4,v:22,e:5"),
        ("long_values", "environment:production-us-east-1,service_name:my-microservice-api,version_number:1.2.3-beta.4,hostname:ip-10-0-1-234.ec2.internal,availability_zone:us-east-1a"),
        ("duplicates", "key1:val1,key2:val2,key1:val1,key3:val3,key2:val2,key4:val4"),
        ("realistic_lambda", "function_arn:arn:aws:lambda:us-east-1:123456789012:function:my-function,runtime:nodejs18.x,architecture:x86_64,env:production,service:api-gateway,version:1.2.3,region:us-east-1,account_id:123456789012"),
    ];
    
    for (name, tags_string) in test_cases {
        group.bench_with_input(BenchmarkId::from_parameter(name), &tags_string, |b, &tags| {
            b.iter(|| {
                SortedTags::parse(black_box(tags))
            });
        });
    }
    
    group.finish();
}

fn benchmark_sorted_tags_parse_lazy(c: &mut Criterion) {
    let mut group = c.benchmark_group("SortedTags::parse_lazy");
    
    let test_cases = vec![
        ("typical", "env:production,service:api,version:1.0.0,host:server01,region:us-east-1"),
        ("realistic_lambda", "function_arn:arn:aws:lambda:us-east-1:123456789012:function:my-function,runtime:nodejs18.x,architecture:x86_64,env:production,service:api-gateway,version:1.2.3,region:us-east-1,account_id:123456789012"),
    ];
    
    for (name, tags_string) in test_cases {
        group.bench_with_input(BenchmarkId::from_parameter(name), &tags_string, |b, &tags| {
            b.iter(|| {
                SortedTags::parse_lazy(black_box(tags))
            });
        });
    }
    
    group.finish();
}

fn benchmark_sorted_tags_parse_presorted(c: &mut Criterion) {
    let mut group = c.benchmark_group("SortedTags::parse_presorted");
    
    let test_cases = vec![
        ("typical_sorted", "env:production,host:server01,region:us-east-1,service:api,version:1.0.0"),
        ("realistic_lambda_sorted", "account_id:123456789012,architecture:x86_64,env:production,function_arn:arn:aws:lambda:us-east-1:123456789012:function:my-function,region:us-east-1,runtime:nodejs18.x,service:api-gateway,version:1.2.3"),
    ];
    
    for (name, tags_string) in test_cases {
        group.bench_with_input(BenchmarkId::from_parameter(name), &tags_string, |b, &tags| {
            b.iter(|| {
                SortedTags::parse_presorted(black_box(tags))
            });
        });
    }
    
    group.finish();
}

fn benchmark_sorted_tags_extend(c: &mut Criterion) {
    let base = SortedTags::parse("a:1,b:2,c:3").unwrap();
    let extension = SortedTags::parse("d:4,e:5,f:6").unwrap();
    
    c.bench_function("SortedTags::extend", |b| {
        b.iter(|| {
            let mut tags = base.clone();
            tags.extend(black_box(&extension));
        });
    });
}

criterion_group!(benches, benchmark_sorted_tags_parse, benchmark_sorted_tags_parse_lazy, benchmark_sorted_tags_parse_presorted, benchmark_sorted_tags_extend);
criterion_main!(benches);
