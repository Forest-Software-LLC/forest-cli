import semver from "semver";

type ResolvedVersions = {
    [packageName : string] : {
        buckets : {
            [version : string] : string[]
        }
        versions : {[version : string] : {
                resolved : boolean,
                dependencies : Record<string, string>
            }
        }
    }
}

type LockfilePackages = { 
    [packageName : string] : {
        version : string, 
        resolved : string,
        integrity : string,
        location : string,
        dependencies : Record<string, string>,
    }[]
}

async function makeRequest(method : "POST" | "GET", url : string) : Promise<any> {
    console.log("Request!")
    return fetch("http://localhost:3000/" + url, {
        method: method,
        headers: {
            'Content-Type': 'application/json'
        },
    }).then(response => {
        if (!response.ok) {
            throw new Error(`HTTP error! status: ${response.status}`);
        }
        return response.json();
    }).catch(error => {
        throw new Error(`Network error: ${error.message}`);
    });

}

export async function getLockfilePackages(rootDeps : Record<string, string>) : Promise<LockfilePackages> {
    let resolved = {} as ResolvedVersions;
    let queue = Object.entries(rootDeps).map(([name, versionRange]) => ({ name, versionRange }));

    while (queue.length > 0) {
        console.log("Queue length: %d", queue.length);
        const { name, versionRange } = queue.shift()!;

        // Initialize the package and fetch its versions if not already done
        if (!resolved[name]) {
            resolved[name] = {
                buckets : {},
                versions : {}
            };

            try {
                const versionsData = await makeRequest("GET", `v1/package/user1/${name}`) as { versions: string[] };

                for (const version of versionsData.versions) {
                    resolved[name].versions[version] = {

                        resolved: false,
                        dependencies: {}
                    };
                }
            } catch (error) {
                throw new Error(`Failed to fetch versions for package ${name}: ${error}`);
            }
        }

        // Get all versions of the package that match the version range
        const allVersions = Object.keys(resolved[name].versions);

        let versionsWithinRange = allVersions.filter(v => semver.satisfies(v, versionRange));
        if (versionsWithinRange.length === 0) {
            throw new Error(`No versions found for ${name} matching range ${versionRange}`);
        }

        // Figure out what bucket this version belongs to
        const buckets = resolved[name].buckets;
        
        let agreedVersion = versionsWithinRange.sort((a, b) => semver.rcompare(a, b))[0]; // Get the latest version within the range
        for (const [bucketVersion, bucketRanges] of Object.entries(buckets)) {
            // Get all versions that are within the bucket version ranges
            
            let allVersionsInBucket = allVersions.filter(v => semver.satisfies(v, versionRange));
            for (const bucketRange of bucketRanges) {
                allVersionsInBucket = allVersionsInBucket.filter(v => semver.satisfies(v, bucketRange));
            }
            
            let newVersion = allVersionsInBucket.sort((a, b) => semver.rcompare(a, b))[0];

            if (newVersion) {
                if (newVersion != bucketVersion) {
                    // If the new version is different from the bucket version, we need to create a new bucket
                    buckets[newVersion] = buckets[bucketVersion].filter(r => r !== versionRange);
                    delete buckets[bucketVersion];
                }

                
                agreedVersion = newVersion;
                break;
            }
        }

        // Create a new bucket if it doesn't exist
        if (!buckets[agreedVersion]) {
            buckets[agreedVersion] = [];
        }

        buckets[agreedVersion].push(versionRange);

        if (resolved[name].versions[agreedVersion].resolved) {
            continue; // Already resolved
        }

        try {
            const packageInfo = await makeRequest("GET", `v1/package/user1/${name}/${agreedVersion}`) as { dependencies?: Record<string, string> };
            const targetVersionInfo = resolved[name].versions[agreedVersion];
            targetVersionInfo.resolved = true;
            targetVersionInfo.dependencies = packageInfo.dependencies || {};
            
            // Add dependencies to the queue
            for (const [depName, depVersion] of Object.entries(packageInfo.dependencies || {})) {
                queue.unshift({ name: depName, versionRange: depVersion });
            }
        } catch (error) {
            throw new Error(`Failed to resolve ${name}@${agreedVersion}:`, error);
        }
    }

    console.log("Resolved versions:", resolved);
    
    let lockfilePackages = {} as LockfilePackages;

    for (const [packageName, packageData] of Object.entries(resolved)) {
        lockfilePackages[packageName] = [];
        for (const bucketVersion of Object.keys(packageData.buckets)) {
            const versionData = packageData.versions[bucketVersion];

            let dependencies = {};
            for (const [depName, depVersionRange] of Object.entries(versionData.dependencies)) {
                let depPackage = resolved[depName];
                let satisfyingVer = Object.keys(depPackage.buckets).find(v => semver.satisfies(v, depVersionRange));

                dependencies[depName] = satisfyingVer
            }
            
            lockfilePackages[packageName].push({
                version: bucketVersion,
                resolved: `https://registry.forestpm.dev/`,
                integrity: "abc-1234",
                dependencies: dependencies,
                location: ""
            });
        }
    }

    function buildTree(name : string, version : string, location : string) {
        const targetPackage = lockfilePackages[name].find(pkg => pkg.version === version);
        if (!targetPackage) {
            throw new Error(`Package ${name} version ${version} not found in lockfile.`);
        }

        if (targetPackage.location !== "" && targetPackage.location.length < (location.length + 1)) {
            return; // Already set to a higher location
        }

        const dependencies = targetPackage.dependencies;
        targetPackage.location = location;
        for (const [depName, depVersion] of Object.entries(dependencies)) {
            buildTree(depName, depVersion, targetPackage.location + "/" + name);
        }
    }

    for (const [name, versionRange] of Object.entries(rootDeps)) {
        const bucketVersions = Object.keys(resolved[name].buckets);
        if (bucketVersions.length === 0) {
            throw new Error(`No versions found for ${name} matching range ${versionRange}`);
        }

        const agreedVersion = bucketVersions.sort((a, b) => semver.rcompare(a, b))[0]; // Get the latest version within the range
        buildTree(name, agreedVersion, "~");
    }

    console.log("Lockfile packages:", lockfilePackages);

    return lockfilePackages;
}

getLockfilePackages({
    "test-2a": "^0.1.0",
    "test-3a": "^0.1.0",
    "test-b": "^0.1.0"
})